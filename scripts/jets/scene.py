"""FBX scene -> world-space parts (vertices already through every transform)."""
import math
import numpy as np
from fbx import load, props70


def rx(a):
    c, s = math.cos(a), math.sin(a)
    return np.array([[1, 0, 0, 0], [0, c, -s, 0], [0, s, c, 0], [0, 0, 0, 1]])


def ry(a):
    c, s = math.cos(a), math.sin(a)
    return np.array([[c, 0, s, 0], [0, 1, 0, 0], [-s, 0, c, 0], [0, 0, 0, 1]])


def rz(a):
    c, s = math.cos(a), math.sin(a)
    return np.array([[c, -s, 0, 0], [s, c, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]])


def euler(v):
    x, y, z = (math.radians(a) for a in v)
    return rz(z) @ ry(y) @ rx(x)  # eEulerXYZ


def tr(v):
    m = np.eye(4)
    m[:3, 3] = v
    return m


def sc(v):
    return np.diag([v[0], v[1], v[2], 1.0])


def vec(p, k, d=(0.0, 0.0, 0.0)):
    v = p.get(k)
    return np.array(v[-3:], dtype=float) if v else np.array(d, dtype=float)


def local_matrix(p):
    T = tr(vec(p, "Lcl Translation"))
    R = euler(vec(p, "Lcl Rotation"))
    S = sc(vec(p, "Lcl Scaling", (1, 1, 1)))
    Rpre = euler(vec(p, "PreRotation"))
    Rpost = euler(vec(p, "PostRotation"))
    Roff, Rp = tr(vec(p, "RotationOffset")), tr(vec(p, "RotationPivot"))
    Soff, Sp = tr(vec(p, "ScalingOffset")), tr(vec(p, "ScalingPivot"))
    return T @ Roff @ Rp @ Rpre @ R @ np.linalg.inv(Rpost) @ np.linalg.inv(Rp) @ Soff @ Sp @ S @ np.linalg.inv(Sp)


def geo_matrix(p):
    return tr(vec(p, "GeometricTranslation")) @ euler(vec(p, "GeometricRotation")) @ sc(vec(p, "GeometricScaling", (1, 1, 1)))


def layer(geo, name, key):
    el = geo.find(name)
    if el is None:
        return None
    mapping = el.find("MappingInformationType").props[0]
    ref = el.find("ReferenceInformationType").props[0]
    data = el.find(key).props[0]
    idx = None
    if ref in ("IndexToDirect", "Index"):
        ik = {"Normals": "NormalsIndex", "UV": "UVIndex", "Materials": None}[key]
        if ik:
            idx = el.find(ik).props[0]
    return mapping, data, idx


class Part:
    def __init__(self, name, pos, nrm, uv, mat, materials, world):
        self.name, self.pos, self.nrm, self.uv, self.mat = name, pos, nrm, uv, mat
        self.materials = materials  # names, index by self.mat
        self.world = world  # node world matrix (pivot frame)


def parts(path):
    root = load(path)
    objs = root.find("Objects")
    byid = {o.props[0]: o for o in objs.children}
    conns = root.find("Connections").children
    children = {}
    for c in conns:
        children.setdefault(c.props[2], []).append((c.props[1], c.props[0], c.props[3] if len(c.props) > 3 else None))
    tex_name = {}
    for oid, o in byid.items():
        if o.name == "Material":
            for cid, kind, prop in children.get(oid, []):
                t = byid.get(cid)
                if t is not None and t.name == "Texture" and prop == "DiffuseColor":
                    fn = (t.find("RelativeFilename") or t.find("FileName")).props[0]
                    tex_name[oid] = fn.replace("\\", "/").split("/")[-1]
    out = []

    def walk(oid, parent_m):
        o = byid[oid]
        p = props70(o)
        m = parent_m @ local_matrix(p)
        name = o.props[1].split("\x00")[0]
        geo, mats = None, []
        for cid, kind, prop in children.get(oid, []):
            c = byid.get(cid)
            if c is None:
                continue
            if c.name == "Geometry":
                geo = c
            elif c.name == "Material":
                mats.append((c.props[1].split("\x00")[0], tex_name.get(cid)))
        if geo is not None:
            out.append(build(name, geo, m @ geo_matrix(p), mats, m))
        for cid, kind, prop in children.get(oid, []):
            c = byid.get(cid)
            if c is not None and c.name == "Model" and kind == "OO":
                walk(cid, m)

    for cid, kind, prop in children.get(0, []):
        if byid.get(cid) is not None and byid[cid].name == "Model":
            walk(cid, np.eye(4))
    return out


def build(name, geo, M, mats, world):
    verts = geo.find("Vertices").props[0].reshape(-1, 3)
    pvi = geo.find("PolygonVertexIndex").props[0]
    nl = layer(geo, "LayerElementNormal", "Normals")
    ul = layer(geo, "LayerElementUV", "UV")
    ml = layer(geo, "LayerElementMaterial", "Materials")
    corners_v = np.where(pvi < 0, ~pvi, pvi)
    ncorner = len(pvi)
    # per-corner attributes
    def per_corner(l, comp, poly_of_corner):
        mapping, data, idx = l
        data = data.reshape(-1, comp)
        if mapping == "ByPolygonVertex":
            sel = np.arange(ncorner)
        elif mapping in ("ByVertice", "ByVertex"):
            sel = corners_v
        elif mapping == "ByPolygon":
            sel = poly_of_corner
        elif mapping == "AllSame":
            sel = np.zeros(ncorner, dtype=int)
        else:
            raise ValueError(mapping)
        if idx is not None:
            sel = idx[sel]
        return data[sel]

    ends = np.nonzero(pvi < 0)[0]
    poly_of_corner = np.zeros(ncorner, dtype=int)
    start = 0
    for i, e in enumerate(ends):
        poly_of_corner[start:e + 1] = i
        start = e + 1
    nrm = per_corner(nl, 3, poly_of_corner)
    uv = per_corner(ul, 2, poly_of_corner) if ul else np.zeros((ncorner, 2))
    if ml:
        mapping, data, _ = ml
        mat = data[poly_of_corner] if mapping == "ByPolygon" else np.full(ncorner, int(data[0]))
    else:
        mat = np.zeros(ncorner, dtype=int)
    # triangulate (fan)
    tris = []
    start = 0
    for e in ends:
        for k in range(start + 1, e):
            tris.append((start, k, k + 1))
        start = e + 1
    tris = np.array(tris, dtype=np.int64)
    flat = tris.reshape(-1)
    pos = verts[corners_v[flat]]
    pos = (np.c_[pos, np.ones(len(pos))] @ M.T)[:, :3]
    N = np.linalg.inv(M[:3, :3]).T
    n = nrm[flat] @ N.T
    n /= np.linalg.norm(n, axis=1, keepdims=True) + 1e-12
    if np.linalg.det(M[:3, :3]) < 0:
        pos = pos.reshape(-1, 3, 3)[:, ::-1].reshape(-1, 3)
        n = n.reshape(-1, 3, 3)[:, ::-1].reshape(-1, 3)
        uvv = uv[flat].reshape(-1, 3, 2)[:, ::-1].reshape(-1, 2)
    else:
        uvv = uv[flat]
    return Part(name, pos, n, uvv, mat[tris[:, 0]], mats, world)
