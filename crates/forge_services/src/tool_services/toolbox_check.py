"""forge toolbox checker (T17): reads a function source on stdin and prints
JSON {"name": ..., "free": [...], "error": ...}: the single top-level def,
and every name it reads that neither the source nor builtins define."""

import ast
import builtins
import json
import sys


def main():
    source = sys.stdin.read()
    try:
        tree = ast.parse(source)
    except SyntaxError as exc:
        print(json.dumps({"error": "syntax error: %s (line %s)" % (exc.msg, exc.lineno)}))
        return
    defs = [n for n in tree.body if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))]
    if len(defs) != 1:
        print(json.dumps({"error": "source must contain exactly one top-level def or class, found %d" % len(defs)}))
        return
    defined = set(dir(builtins))
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            defined.add(node.name)
        elif isinstance(node, (ast.Import, ast.ImportFrom)):
            for alias in node.names:
                defined.add((alias.asname or alias.name).split(".")[0])
        elif isinstance(node, ast.Name) and isinstance(node.ctx, (ast.Store, ast.Del)):
            defined.add(node.id)
        elif isinstance(node, ast.arg):
            defined.add(node.arg)
        elif isinstance(node, ast.ExceptHandler) and node.name:
            defined.add(node.name)
        elif isinstance(node, (ast.Global, ast.Nonlocal)):
            defined.update(node.names)
        elif isinstance(node, ast.alias):
            pass
    free = sorted({
        node.id for node in ast.walk(tree)
        if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load) and node.id not in defined
    })
    print(json.dumps({"name": defs[0].name, "free": free}))


main()
