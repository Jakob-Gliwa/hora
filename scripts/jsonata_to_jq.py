"""Translate the migration subset of JSONata to pure jq; reject everything else.

No eval, subprocess, network access, or installation-specific expressions.
"""
import ast
import json
import re


class TranslationError(ValueError):
    pass


TOKEN = re.compile(r'''\s*(\$[A-Za-z_]\w*|\$|[A-Za-z_]\w*|\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|:=|!=|<=|>=|==|[><=+\-?:;(),.\[\]])''')
PRECEDENCE = {"or": 1, "and": 2, "in": 3, "=": 3, "==": 3, "!=": 3, "<": 3, "<=": 3, ">": 3, ">=": 3}


class Parser:
    def __init__(self, source):
        self.tokens = []
        pos = 0
        while pos < len(source.rstrip()):
            match = TOKEN.match(source, pos)
            if not match:
                raise TranslationError("unsupported JSONata syntax")
            self.tokens.append(match[1])
            pos = match.end()
        self.tokens.append("EOF")
        self.pos = 0

    def peek(self):
        return self.tokens[self.pos]

    def take(self, expected=None):
        token = self.peek()
        if expected is not None and token != expected:
            raise TranslationError("unsupported JSONata grammar")
        self.pos += 1
        return token

    def parse(self):
        result = self.expression()
        self.take("EOF")
        return result

    def expression(self, minimum=0):
        left = self.atom()
        while self.peek() in PRECEDENCE and PRECEDENCE[self.peek()] >= minimum:
            op = self.take()
            right = self.expression(PRECEDENCE[op] + 1)
            left = ("binary", op, left, right)
        if minimum == 0 and self.peek() == "?":
            self.take()
            yes = self.expression()
            self.take(":")
            left = ("if", left, yes, self.expression())
        return left

    def atom(self):
        token = self.take()
        if token == "(":
            bindings = []
            while self.peek().startswith("$") and self.tokens[self.pos + 1] == ":=":
                name = self.take()
                if name == "$":
                    raise TranslationError("cannot bind the JSONata context")
                self.take(":=")
                bindings.append((name, self.expression()))
                self.take(";")
            node = self.expression()
            self.take(")")
            if bindings:
                node = ("let", bindings, node)
        elif token == "[":
            items = []
            if self.peek() != "]":
                items.append(self.expression())
                while self.peek() == ",":
                    self.take()
                    items.append(self.expression())
            self.take("]")
            node = ("array", items)
        elif token in ("-", "+"):
            node = ("unary", token, self.atom())
        elif token.startswith(('"', "'")):
            node = ("literal", json.loads(token) if token.startswith('"') else ast.literal_eval(token))
        elif token[0].isdigit():
            node = ("literal", json.loads(token))
        elif token in ("true", "false", "null"):
            node = ("literal", json.loads(token))
        elif token == "$":
            node = ("root",)
        elif token.startswith("$"):
            if self.peek() == "(":
                self.take()
                args = []
                if self.peek() != ")":
                    args.append(self.expression())
                    while self.peek() == ",":
                        self.take()
                        args.append(self.expression())
                self.take(")")
                node = ("call", token, args)
            else:
                node = ("var", token)
        elif re.fullmatch(r"[A-Za-z_]\w*", token):
            node = ("field", ("root",), token)
        else:
            raise TranslationError("unsupported JSONata expression")
        while self.peek() in (".", "["):
            if self.take() == ".":
                name = self.take()
                if not re.fullmatch(r"[A-Za-z_]\w*", name):
                    raise TranslationError("unsupported JSONata path")
                node = ("field", node, name)
            else:
                selector = self.expression()
                self.take("]")
                numeric = selector[0] == "literal" and type(selector[1]) is int
                node = ("index" if numeric else "filter", node, selector)
        return node


def exists(node):
    """Keep missing paths distinct from explicit JSON null for $exists."""
    kind = node[0]
    if kind == "var":
        return "$__defined_" + node[1][1:]
    if kind in ("field", "index"):
        base = emit(node[1])
        key = json.dumps(node[2]) if kind == "field" else emit(node[2])
        expected_type = "object" if kind == "field" else "array"
        return f'({base} | if type == "{expected_type}" then has({key}) else false end)'
    if kind in ("root", "literal", "array", "filter"):
        return "true"
    raise TranslationError("existence checks require a path or bound variable")


def emit(node):
    kind = node[0]
    if kind == "root":
        return "."
    if kind == "literal":
        return json.dumps(node[1], ensure_ascii=False)
    if kind == "var":
        return node[1]
    if kind == "field":
        return f'({emit(node[1])} | if type == "object" then .[{json.dumps(node[2])}] else null end)'
    if kind == "index":
        return f"({emit(node[1])})[{emit(node[2])}]"
    if kind == "filter":
        return f"[({emit(node[1])} | if type == \"array\" then .[] else . end) | select({emit(node[2])})]"
    if kind == "array":
        return "[" + ", ".join(map(emit, node[1])) + "]"
    if kind == "unary":
        return f"({node[1]}({emit(node[2])}))"
    if kind == "if":
        return f"(if {emit(node[1])} then {emit(node[2])} else {emit(node[3])} end)"
    if kind == "let":
        return "(" + " | ".join(f"({exists(value)}) as $__defined_{name[1:]} | ({emit(value)}) as {name}" for name, value in node[1]) + " | " + emit(node[2]) + ")"
    if kind == "binary":
        op, left, right = node[1], emit(node[2]), emit(node[3])
        if op in ("and", "or", "=", "==", "!="):
            return f"(({left}) {'==' if op == '=' else op} ({right}))"
        if op == "in":
            return f"(({left}) as $__item | ({right}) | index($__item) != null)"
        # jq's ordering between JSON types differs from JSONata. Missing data
        # must not become a passing comparison because an object sorts high.
        return f'(({left}) as $__a | ({right}) as $__b | if ({exists(node[2])}) and ({exists(node[3])}) then (if (($__a | type) == ($__b | type)) and (($__a | type) == "number" or ($__a | type) == "string") then $__a {op} $__b else error("incompatible comparison") end) else false end)'
    if kind == "call":
        name, args = node[1:]
        if name == "$number" and len(args) == 1:
            return f'({emit(args[0])} | if . == true then 1 elif . == false then 0 else tonumber end)'
        if name == "$exists" and len(args) == 1:
            return exists(args[0])
        if name == "$count" and len(args) == 1:
            return f'({emit(args[0])} | if type == "array" then length else 1 end)'
        if name == "$round" and len(args) in (1, 2):
            precision = args[1] if len(args) == 2 else ("literal", 0)
            if precision[0] != "literal" or type(precision[1]) is not int or not 0 <= precision[1] <= 6:
                raise TranslationError("round precision must be a literal integer in 0..6")
            factor = 10 ** precision[1]
            # JSONata uses round-half-to-even, unlike jq's round at a tie.
            return f'({emit(args[0])} | . * {factor} | floor as $__floor | if . - $__floor == 0.5 then (if $__floor % 2 == 0 then $__floor else $__floor + 1 end) else round end | . / {factor})'
        raise TranslationError("unsupported JSONata function or arguments")
    raise TranslationError("unsupported JSONata node")


def translate(source):
    node = Parser(source).parse()
    validate_bindings(node, set())
    return emit(node)


def validate_bindings(node, bound):
    if node[0] == "var" and node[1] not in bound:
        raise TranslationError("unbound JSONata variable")
    if node[0] == "let":
        bound = set(bound)
        for name, value in node[1]:
            if name.startswith("$__"):
                raise TranslationError("variable name reserved by the translator")
            validate_bindings(value, bound)
            bound.add(name)
        validate_bindings(node[2], bound)
        return
    for child in node[1:]:
        if isinstance(child, tuple): validate_bindings(child, bound)
        if isinstance(child, list):
            for item in child: validate_bindings(item, bound)
