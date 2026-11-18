"""Minimal binary FBX (7.x) reader."""
import struct, zlib
import numpy as np


class Node:
    __slots__ = ("name", "props", "children")

    def __init__(self, name, props, children):
        self.name, self.props, self.children = name, props, children

    def find(self, name):
        for c in self.children:
            if c.name == name:
                return c
        return None

    def findall(self, name):
        return [c for c in self.children if c.name == name]

    def __repr__(self):
        return f"Node({self.name}, {self.props[:3]})"


ARR = {b"f": "<f4", b"d": "<f8", b"l": "<i8", b"i": "<i4", b"b": "u1"}


def _prop(buf, o):
    t = buf[o:o + 1]
    o += 1
    if t == b"Y":
        return struct.unpack_from("<h", buf, o)[0], o + 2
    if t == b"C":
        return buf[o] != 0, o + 1
    if t == b"I":
        return struct.unpack_from("<i", buf, o)[0], o + 4
    if t == b"F":
        return struct.unpack_from("<f", buf, o)[0], o + 4
    if t == b"D":
        return struct.unpack_from("<d", buf, o)[0], o + 8
    if t == b"L":
        return struct.unpack_from("<q", buf, o)[0], o + 8
    if t in (b"S", b"R"):
        n = struct.unpack_from("<I", buf, o)[0]
        o += 4
        s = buf[o:o + n]
        return (s if t == b"R" else s.decode("utf-8", "replace")), o + n
    if t in ARR:
        n, enc, clen = struct.unpack_from("<III", buf, o)
        o += 12
        raw = buf[o:o + clen]
        if enc == 1:
            raw = zlib.decompress(raw)
        return np.frombuffer(raw, dtype=ARR[t], count=n).copy(), o + clen
    raise ValueError(f"bad prop type {t} at {o}")


def _node(buf, o, ver):
    if ver >= 7500:
        end, nprops, plen = struct.unpack_from("<QQQ", buf, o)
        o += 24
    else:
        end, nprops, plen = struct.unpack_from("<III", buf, o)
        o += 12
    nlen = buf[o]
    o += 1
    if end == 0:
        return None, o
    name = buf[o:o + nlen].decode()
    o += nlen
    props = []
    for _ in range(nprops):
        p, o = _prop(buf, o)
        props.append(p)
    children = []
    sentinel = 25 if ver >= 7500 else 13
    while o < end:
        if end - o == sentinel and buf[o:end] == b"\0" * sentinel:
            o = end
            break
        c, o = _node(buf, o, ver)
        if c is None:
            break
        children.append(c)
    return Node(name, props, children), end


def load(path):
    buf = open(path, "rb").read()
    ver = struct.unpack_from("<I", buf, 23)[0]
    o = 27
    top = []
    while o < len(buf) - 200:
        n, o = _node(buf, o, ver)
        if n is None:
            break
        top.append(n)
    return Node("root", [], top)


def props70(node):
    out = {}
    p = node.find("Properties70")
    if p:
        for c in p.children:
            out[c.props[0]] = c.props[4:]
    return out
