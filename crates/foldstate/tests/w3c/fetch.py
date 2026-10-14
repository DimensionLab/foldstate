# SPDX-License-Identifier: Apache-2.0
"""Download the W3C SCXML 1.0 implementation report tests and convert the ones foldstate can
express into definitions.

    python3 crates/foldstate/tests/w3c/fetch.py

Writes `cache/cases.json` next to this file (the tests themselves stay in `cache/` and are not
part of the repository). `tests/w3c.rs` runs the cases.

The tests are written against an abstract data model (`conf:` attributes). What maps onto
foldstate's guards and actions is converted: numeric variables, comparisons, `In`,
assignment, increment, raise, send to the machine itself (with delay and cancel). Everything
else makes a test "not applicable", with the construct as the reason.
"""
import json
import re
import sys
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

BASE = "https://www.w3.org/Voice/2013/scxml-irp/"
HERE = Path(__file__).parent
CACHE = HERE / "cache"
SC = "{http://www.w3.org/2005/07/scxml}"
CF = "{http://www.w3.org/2005/scxml-conformance}"
STATE_TAGS = {SC + "state": None, SC + "parallel": "parallel", SC + "final": "final", SC + "history": "history"}


class NotApplicable(Exception):
    pass


def fetch(uri: str) -> bytes:
    path = CACHE / uri
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        with urllib.request.urlopen(BASE + uri, timeout=60) as response:
            path.write_bytes(response.read())
    return path.read_bytes()


def local(tag: str) -> str:
    return tag.split("}")[-1]


def conf_attrs(el):
    return {local(k): v for k, v in el.attrib.items() if k.startswith(CF)}


def var(n: str) -> str:
    return f"/Var{n.strip()}"


def number(text: str):
    text = text.strip()
    if re.fullmatch(r"-?\d+", text):
        return int(text)
    raise NotApplicable(f"expression `{text}`")


OPS = {"=": "eq", "==": "eq", "!=": "ne", "<": "lt", ">": "gt", "<=": "le", ">=": "ge"}


class Converter:
    def __init__(self, root):
        self.root = root
        self.paths = {}
        self.anonymous = 0

    def state_id(self, el) -> str:
        if el.tag == CF + "pass":
            return "pass"
        if el.tag == CF + "fail":
            return "fail"
        sid = el.get("id")
        if sid is None:
            self.anonymous += 1
            sid = f"anonymous{self.anonymous}"
            el.set("id", sid)
        if not re.fullmatch(r"[A-Za-z0-9_\-]+", sid):
            raise NotApplicable(f"state id `{sid}`")
        return sid

    def index(self, el, path):
        for child in el:
            if child.tag in STATE_TAGS or child.tag in (CF + "pass", CF + "fail"):
                sid = self.state_id(child)
                child_path = sid if path is None else f"{path}.{sid}"
                if sid in self.paths:
                    raise NotApplicable("duplicate state id")
                self.paths[sid] = child_path
                self.index(child, child_path)

    def ref(self, sid: str) -> str:
        if sid not in self.paths:
            raise NotApplicable(f"target `{sid}`")
        return "#" + self.paths[sid]

    def guard(self, el):
        if el.get("cond") is not None:
            raise NotApplicable("cond expression")
        conf = {k: v for k, v in conf_attrs(el).items() if k not in ("targetpass", "targetfail")}
        if not conf:
            return None
        if len(conf) > 1:
            raise NotApplicable("several conf: conditions")
        (name, value), = conf.items()
        if name == "true":
            return None
        if name == "false":
            return {"cmp": {"left": {"lit": 0}, "op": "eq", "right": {"lit": 1}}}
        if name == "inState":
            return {"in": self.ref(value)}
        if name in ("idVal", "compareIDVal"):
            m = re.fullmatch(r"\s*(\d+)\s*(==|!=|<=|>=|=|<|>)\s*(-?\d+)\s*", value)
            if not m:
                raise NotApplicable(f"conf:{name}=`{value}`")
            right = {"ctx": var(m.group(3))} if name == "compareIDVal" else {"lit": int(m.group(3))}
            return {"cmp": {"left": {"ctx": var(m.group(1))}, "op": OPS[m.group(2)], "right": right}}
        if name == "VarEqVar":
            a, b = value.split()
            return {"cmp": {"left": {"ctx": var(a)}, "op": "eq", "right": {"ctx": var(b)}}}
        raise NotApplicable(f"conf:{name}")

    def actions(self, el):
        out = []
        for a in el:
            tag = a.tag
            conf = conf_attrs(a)
            if tag == SC + "log" or tag is ET.Comment:
                continue
            if tag == SC + "raise":
                out.append({"raise": {"event": a.get("event")}})
            elif tag == SC + "assign":
                if set(conf) == {"location", "expr"}:
                    value = {"lit": number(conf["expr"])}
                elif set(conf) == {"location", "varExpr"}:
                    value = {"ctx": var(conf["varExpr"])}
                else:
                    raise NotApplicable("assign with " + ", ".join(sorted("conf:" + c for c in conf) or ["expr"]))
                out.append({"assign": {"path": var(conf["location"]), "value": value}})
            elif tag == CF + "incrementID":
                v = var(a.get("id"))
                out.append({"assign": {"path": v, "value": {"add": [{"ctx": v}, {"lit": 1}]}}})
            elif tag == CF + "sumVars":
                v = var(a.get("id1"))
                out.append({"assign": {"path": v, "value": {"add": [{"ctx": v}, {"ctx": var(a.get("id2"))}]}}})
            elif tag == SC + "send":
                if len(a):
                    raise NotApplicable("send with content or parameters")
                plain = dict(a.attrib)
                event, sid, target = plain.pop("event", None), plain.pop("id", None), plain.pop("target", None)
                delay = conf.pop("delay", None)
                literal = plain.pop("delay", None)
                if literal is not None:
                    m = re.fullmatch(r"\s*([0-9.]+)(ms|s)\s*", literal)
                    if not m:
                        raise NotApplicable(f"delay `{literal}`")
                    delay = float(m.group(1)) / (1000.0 if m.group(2) == "ms" else 1.0)
                for k in list(plain):
                    if k.startswith(CF):
                        plain.pop(k)
                if plain or conf or event is None:
                    raise NotApplicable("send with " + ", ".join(sorted(list(plain) + ["conf:" + c for c in conf]) or ["no event"]))
                if target == "#_internal":
                    out.append({"raise": {"event": event}})
                elif target is None:
                    params = {"event": event, "delay": float(delay) if delay else 0.0, "id": sid}
                    out.append({"emit": {"name": "send", "params": {"lit": params}}})
                else:
                    raise NotApplicable(f"send to `{target}`")
            elif tag == SC + "cancel":
                if a.get("sendid") is None:
                    raise NotApplicable("cancel by expression")
                out.append({"emit": {"name": "cancel", "params": {"lit": {"id": a.get("sendid")}}}})
            else:
                raise NotApplicable(local(tag))
        return out

    def descriptors(self, text: str) -> str:
        out = []
        for token in text.split():
            token = re.sub(r"\.\*$|\.$", "", token)
            if token == "*":
                out.append("*")
                continue
            m = re.fullmatch(r"done\.state\.(.+)", token)
            if m and m.group(1) in self.paths:
                out.append("done.state." + self.paths[m.group(1)])
                continue
            if token.startswith(("error", "done.invoke")):
                raise NotApplicable(f"event `{token}`")
            # SCXML: a descriptor matches every event name it is a prefix of.
            out += [token, token + ".*"]
        return " ".join(out)

    def transition(self, el):
        t = {}
        if el.get("event") is not None:
            t["event"] = self.descriptors(el.get("event"))
        guard = self.guard(el)
        if guard is not None:
            t["guard"] = guard
        targets = [self.ref(s) for s in (el.get("target") or "").split()]
        if CF + "targetpass" in el.attrib:
            targets.append("#pass")
        if CF + "targetfail" in el.attrib:
            targets.append("#fail")
        if targets:
            t["target"] = targets
        if el.get("type") == "internal":
            t["internal"] = True
        actions = self.actions(el)
        if actions:
            t["actions"] = actions
        return t

    def initial(self, el, children):
        """The id of the default child, from the `initial` attribute or element."""
        names = (el.get("initial") or "").split()
        for child in el:
            if child.tag == SC + "initial":
                (tr,) = list(child)
                if len(tr):
                    raise NotApplicable("initial with executable content")
                names = (tr.get("target") or "").split()
        if not names:
            return None
        if len(names) != 1 or names[0] not in children:
            raise NotApplicable("initial that is not one child state")
        return names[0]

    def state(self, el, root=False):
        tag = el.tag
        s = {"id": "scxml" if root else self.state_id(el)}
        if tag in (CF + "pass", CF + "fail"):
            s["kind"] = "final"
            return s
        kind = None if root else STATE_TAGS[tag]
        if kind:
            s["kind"] = kind
        if kind == "history":
            if el.get("type") == "deep":
                s["deep"] = True
            for child in el:
                if child.tag == SC + "transition":
                    if len(child):
                        raise NotApplicable("history default with executable content")
                    s["default"] = [self.ref(t) for t in child.get("target").split()]
            return s
        states, on, entry, exit_ = [], [], [], []
        children = []
        for child in el:
            ct = child.tag
            if ct in STATE_TAGS or ct in (CF + "pass", CF + "fail"):
                states.append(self.state(child))
                if ct != SC + "history":
                    children.append(states[-1]["id"])
            elif ct == SC + "transition":
                on.append(self.transition(child))
            elif ct == SC + "onentry":
                entry += self.actions(child)
            elif ct == SC + "onexit":
                exit_ += self.actions(child)
            elif ct in (SC + "initial", SC + "datamodel"):
                pass
            elif ct is ET.Comment:
                pass
            else:
                raise NotApplicable(local(ct))
        initial = self.initial(el, children)
        if initial:
            s["initial"] = initial
        if states:
            s["states"] = states
        if on:
            s["on"] = on
        if entry:
            s["entry"] = entry
        if exit_:
            s["exit"] = exit_
        return s

    def context(self):
        ctx = {}
        for data in self.root.iter(SC + "data"):
            conf = conf_attrs(data)
            if len(data) or data.text and data.text.strip() or data.get("src") or data.get("expr"):
                raise NotApplicable("data with content")
            if set(conf) == {"id"}:
                ctx["Var" + conf["id"]] = None
            elif set(conf) == {"id", "expr"}:
                ctx["Var" + conf["id"]] = number(conf["expr"])
            else:
                raise NotApplicable("data with " + ", ".join(sorted("conf:" + c for c in conf)))
        return ctx

    def definition(self):
        root = self.root
        if root.get("binding") == "late":
            raise NotApplicable("late binding")
        extra = set(conf_attrs(root)) - {"datamodel"}
        if extra:
            raise NotApplicable("conf:" + sorted(extra)[0])
        self.index(root, None)
        d = {
            "schema": "foldstate/1",
            "id": "w3c",
            "context": self.context(),
            "unhandled": "ignore",
            "root": self.state(root, root=True),
        }
        return d


def main():
    CACHE.mkdir(exist_ok=True)
    manifest = ET.fromstring(fetch("manifest.xml"))
    cases = []
    for test in manifest.iter("test"):
        start = test.find("start").get("uri")
        case = {
            "id": test.get("id"),
            "uri": start,
            "conformance": test.get("conformance"),
            "manual": test.get("manual") == "true",
            "not_applicable": None,
            "definition": None,
        }
        if not start.endswith(".txml"):
            case["not_applicable"] = "not a txml test"
        elif test.findall("dep"):
            case["not_applicable"] = "needs auxiliary files"
        elif case["manual"]:
            case["not_applicable"] = "manual test"
        else:
            try:
                case["definition"] = Converter(ET.fromstring(fetch(start))).definition()
            except NotApplicable as reason:
                case["not_applicable"] = str(reason)
            except ET.ParseError as error:
                case["not_applicable"] = f"not well-formed XML: {error}"
        cases.append(case)
    (CACHE / "cases.json").write_text(json.dumps(cases, indent=1))
    converted = sum(1 for c in cases if c["definition"])
    print(f"{len(cases)} tests, {converted} converted, {len(cases) - converted} not applicable", file=sys.stderr)


if __name__ == "__main__":
    main()
