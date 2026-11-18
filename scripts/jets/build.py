"""Builds assets/models/jets/: one .bin per jet, one .gltf per livery, shared textures.

usage: python3 -E build.py <fbx dir> <pack Models dir> <out dir>

Control surfaces get their own node, re-pivoted so the node origin sits on the
hinge line and local +X is the hinge axis, oriented so a positive rotation
moves the trailing edge down (horizontal surfaces) or to port (rudders).
"""
import json, math, os, struct, sys
import numpy as np
from PIL import Image
from scene import parts

FBX, PACK, OUT = sys.argv[1:4]
SCALE = 1.1  # the old F-22 was ~1.1x life size; keep the jets' on-screen size
TEX = 1024

# part name -> role. Unlisted parts merge into the airframe.
PLANES = {
    "f16": {
        "fbx": "F16.FBX",
        "liveries": {"military": "f16-defuse-military.png", "arctic": "f16-defuse-arctic.png", "thunderbirds": "f16-defuse-thunderbirds.png"},
        "normal": "f16_normal.png", "spec": "f16_spec.jpg", "cockpit": "f16_cockpit.png",
        "roles": {
            "elevatorl512": ("stab_l", "allmoving"), "elevatorr512": ("stab_r", "allmoving"),
            "flapl512": ("flap_l", "hinged"), "flapr512": ("flap_r", "hinged"),
            "rudder512": ("rudder", "hinged"),
            "vc512": ("cockpit", None), "vchud512": ("hud", None),
            "canopy512": ("canopy", None), "canopyGlass512": ("canopy", None),
            "CanopyInt512": ("canopy_inner", None),
        },
        "nozzle": lambda n: "nozzle" if n.startswith("nozzle") else None,
    },
    "f18": {
        "fbx": "F18.FBX",
        "liveries": {"military": "f18-defuse-military.png", "tiger": "f18-defuse-tiger.png"},
        "normal": "f18_normal.png", "spec": "f18_spec.jpg", "cockpit": "f18_cockpit.png",
        "roles": {
            "leftelevator512": ("stab_l", "allmoving"), "rightelevator512": ("stab_r", "allmoving"),
            "leftaileron512": ("aileron_l", "hinged"), "rightaileron512": ("aileron_r", "hinged"),
            "leftrudder512": ("rudder_l", "hinged"), "rightrudder512": ("rudder_r", "hinged"),
            "vs512": ("cockpit", None), "vchud512": ("hud", None),
            "canopy512": ("canopy", None), "canopyint512": ("canopy_inner", None),
        },
        "nozzle": lambda n: ("nozzle_r" if n.startswith("rnozzle") else "nozzle_l") if "nozzle" in n else None,
    },
    "su37": {
        "fbx": "Su37.FBX",
        "liveries": {"military": "su37-defuse-russmilitary.png"},
        "normal": "su37_normal.png", "spec": "su37_spec.jpg", "cockpit": "f16_cockpit.png",
        "roles": {
            "elevatorl512": ("stab_l", "allmoving"), "elevatorr512": ("stab_r", "allmoving"),
            "flapl512": ("flap_l", "hinged"), "flapr512": ("flap_r", "hinged"),
            "canardl512": ("canard_l", "allmoving"), "canardr512": ("canard_r", "allmoving"),
            "rudderl512": ("rudder_l", "hinged"), "rudderr512": ("rudder_r", "hinged"),
            "vc512": ("cockpit", None), "vchud512": ("hud", None),
            "canopyGlass512": ("canopy", None), "cockpit512": ("cockpit_tub", None),
        },
        "nozzle": lambda n: {"enginevecl512": "nozzle_l", "nozzlel512": "nozzle_l", "enginevecr512": "nozzle_r", "nozzler512": "nozzle_r"}.get(n),
    },
}


def pca(pts):
    c = pts.mean(0)
    w, v = np.linalg.eigh(np.cov((pts - c).T))
    return c, v[:, np.argsort(w)]  # columns: smallest .. largest variance


def hinge(pts, kind, vertical):
    """(point, axis) of a control surface's hinge."""
    c, ev = pca(pts)
    n = ev[:, 0]
    chord = np.array([0.0, 0.0, 1.0])
    chord = chord - n * n.dot(chord)
    chord /= np.linalg.norm(chord)
    span = np.cross(n, chord)
    s = (pts - c) @ span
    z = (pts - c) @ chord
    if kind == "allmoving":
        # spindle: straight out from the fuselage (no sweep), ~40% of the root chord
        w = s.max() - s.min()
        lo_end, hi_end = s < s.min() + 0.25 * w, s > s.max() - 0.25 * w
        root = lo_end if abs(pts[lo_end, 0]).mean() < abs(pts[hi_end, 0]).mean() else hi_end
        zr = z[root]
        z0 = zr.min() + 0.4 * (zr.max() - zr.min())
        p = c + chord * z0
        axis = span
    else:
        # hinge line along the leading edge: fit the front-most point per span slice
        bins = np.linspace(s.min(), s.max(), 9)
        ss, zz = [], []
        for a, b in zip(bins[:-1], bins[1:]):
            m = (s >= a) & (s <= b)
            if m.sum():
                i = np.argmin(np.where(m, z, np.inf))
                ss.append(s[i])
                zz.append(z[i])
        k, z0 = np.polyfit(ss, zz, 1)
        axis = span + chord * k
        axis /= np.linalg.norm(axis)
        p = c + chord * (z0 + k * 0.0)
    # orient: a positive rotation moves the trailing edge down / to port
    te = c + chord * z.max()
    move = np.cross(axis, te - p)
    want = np.array([-1.0, 0, 0]) if vertical else np.array([0, -1.0, 0])
    if move.dot(want) < 0:
        axis = -axis
    return p, axis


def frame(axis):
    x = axis / np.linalg.norm(axis)
    up = np.array([0, 1.0, 0]) if abs(x[1]) < 0.9 else np.array([0, 0, 1.0])
    z = np.cross(x, up)
    z /= np.linalg.norm(z)
    y = np.cross(z, x)
    return np.stack([x, y, z], axis=1)  # columns


def quat(R):
    t = np.trace(R)
    if t > 0:
        s = math.sqrt(t + 1) * 2
        w, x, y, z = 0.25 * s, (R[2, 1] - R[1, 2]) / s, (R[0, 2] - R[2, 0]) / s, (R[1, 0] - R[0, 1]) / s
    else:
        i = int(np.argmax(np.diag(R)))
        if i == 0:
            s = math.sqrt(1 + R[0, 0] - R[1, 1] - R[2, 2]) * 2
            w, x, y, z = (R[2, 1] - R[1, 2]) / s, 0.25 * s, (R[0, 1] + R[1, 0]) / s, (R[0, 2] + R[2, 0]) / s
        elif i == 1:
            s = math.sqrt(1 + R[1, 1] - R[0, 0] - R[2, 2]) * 2
            w, x, y, z = (R[0, 2] - R[2, 0]) / s, (R[0, 1] + R[1, 0]) / s, 0.25 * s, (R[1, 2] + R[2, 1]) / s
        else:
            s = math.sqrt(1 + R[2, 2] - R[0, 0] - R[1, 1]) * 2
            w, x, y, z = (R[1, 0] - R[0, 1]) / s, (R[0, 2] + R[2, 0]) / s, (R[1, 2] + R[2, 1]) / s, 0.25 * s
    return [float(x), float(y), float(z), float(w)]


class Gltf:
    def __init__(self):
        self.bin = bytearray()
        self.views, self.accessors, self.meshes, self.nodes = [], [], [], []

    def add(self, arr, comp, typ, target=None, minmax=False):
        while len(self.bin) % 4:
            self.bin += b"\0"
        off = len(self.bin)
        self.bin += arr.tobytes()
        view = {"buffer": 0, "byteOffset": off, "byteLength": arr.nbytes}
        if target:
            view["target"] = target
        self.views.append(view)
        acc = {"bufferView": len(self.views) - 1, "componentType": comp, "count": len(arr), "type": typ}
        if minmax:
            acc["min"] = arr.min(0).tolist()
            acc["max"] = arr.max(0).tolist()
        self.accessors.append(acc)
        return len(self.accessors) - 1

    def mesh(self, name, prims):
        """prims: list of (material index, pos, nrm, uv) as unindexed triangles."""
        out = []
        for mat, pos, nrm, uv in prims:
            # weld identical corners
            key = np.round(np.c_[pos, nrm, uv], 5)
            uniq, inv = np.unique(key, axis=0, return_inverse=True)
            inv = inv.reshape(-1)
            p = uniq[:, 0:3].astype(np.float32)
            n = uniq[:, 3:6].astype(np.float32)
            n /= np.linalg.norm(n, axis=1, keepdims=True) + 1e-9
            t = uniq[:, 6:8].astype(np.float32)
            idx = inv.astype(np.uint16 if len(uniq) < 65535 else np.uint32)
            out.append({
                "attributes": {
                    "POSITION": self.add(p, 5126, "VEC3", 34962, True),
                    "NORMAL": self.add(n, 5126, "VEC3", 34962),
                    "TEXCOORD_0": self.add(t, 5126, "VEC2", 34962),
                },
                "indices": self.add(idx, 5123 if idx.dtype == np.uint16 else 5125, "SCALAR", 34963),
                "material": mat,
            })
        self.meshes.append({"name": name, "primitives": out})
        return len(self.meshes) - 1


def save_tex(src, dst, size, quality=88, mode="RGB"):
    im = Image.open(src).convert(mode)
    if size and im.size[0] > size:
        im = im.resize((size, size * im.size[1] // im.size[0]), Image.LANCZOS)
    if dst.endswith(".png"):
        im.save(dst, optimize=True)
    else:
        im.save(dst, quality=quality, optimize=True)


def build(key, cfg):
    ps = parts(os.path.join(FBX, cfg["fbx"]))
    allp = np.concatenate([p.pos for p in ps])
    lo, hi = allp.min(0), allp.max(0)
    L = hi[2] - lo[2]
    noz = [p for p in ps if cfg["nozzle"](p.name)]
    nozc = np.concatenate([p.pos for p in noz])
    origin = np.array([0.0, (nozc[:, 1].min() + nozc[:, 1].max()) / 2 - 0.1, (lo[2] + hi[2]) / 2 + 0.07 * L])

    def tx(v):
        return (v - origin) * SCALE

    # material slots: 0 body, 1 nozzle (f16), 2 glass, 3 cockpit, 4 hud
    def slot(mname, tex):
        t = (tex or "").lower()
        m = mname.lower()
        if "glassint" in m or "glassint" in t:
            return 5
        if "glass" in m or "glass" in t:
            return 2
        if "hud" in m or "hud" in t or m == "13 - default":
            return 4
        if "cockpit" in m or "cockpit" in t or m == "14 - default" or t == "vc.jpg":
            return 3
        if "nozzle" in m:
            return 1
        return 0

    groups = {}  # node name -> {slot: [pos, nrm, uv]}
    kinds = {}
    for p in ps:
        role, kind = cfg["roles"].get(p.name, ("airframe", None))
        nz = cfg["nozzle"](p.name)
        if nz:
            role, kind = nz, "nozzle"
        if role is None:
            continue
        kinds[role] = kind
        g = groups.setdefault(role, {})
        tri_mat = p.mat
        for mi in np.unique(tri_mat):
            mname, tex = p.materials[mi] if mi < len(p.materials) else p.materials[0]
            s = slot(mname, tex)
            sel = np.repeat(tri_mat == mi, 3)
            acc = g.setdefault(s, [[], [], []])
            acc[0].append(tx(p.pos[sel]))
            acc[1].append(p.nrm[sel])
            acc[2].append(np.c_[p.uv[sel][:, 0], 1.0 - p.uv[sel][:, 1]])

    gl = Gltf()
    info = {"length": float(L * SCALE), "span": float((hi[0] - lo[0]) * SCALE), "surfaces": {}, "nozzles": {}}
    for role in sorted(groups):
        g = groups[role]
        pts = np.concatenate([np.concatenate(v[0]) for v in g.values()])
        node = {"name": role}
        kind = kinds[role]
        if kind in ("hinged", "allmoving"):
            vertical = role.startswith("rudder")
            p, axis = hinge(pts, kind, vertical)
            R = frame(axis)
            node["translation"] = p.tolist()
            node["rotation"] = quat(R)
            xf = lambda v: (v - p) @ R
            nf = lambda n: n @ R
            info["surfaces"][role] = {"pivot": p.round(3).tolist(), "axis": axis.round(3).tolist(),
                                      "span": float(np.ptp(((pts - p) @ R)[:, 0]))}
        elif kind == "nozzle":
            # node at the petal ring's centre, at the front of the nozzle
            c = np.array([(pts[:, 0].min() + pts[:, 0].max()) / 2, (pts[:, 1].min() + pts[:, 1].max()) / 2, pts[:, 2].min()])
            node["translation"] = c.tolist()
            xf = lambda v: v - c
            nf = lambda n: n
            info["nozzles"][role] = {"front": c.round(3).tolist(), "exit_z": round(float(pts[:, 2].max()), 3),
                                     "radius": round(float(max(np.ptp(pts[:, 0]), np.ptp(pts[:, 1])) / 2), 3)}
        else:
            xf = lambda v: v
            nf = lambda n: n
        prims = []
        for s, (P, N, U) in sorted(g.items()):
            prims.append((s, xf(np.concatenate(P)), nf(np.concatenate(N)), np.concatenate(U)))
        node["mesh"] = gl.mesh(role, prims)
        gl.nodes.append(node)

    # handy points for effects (aircraft space, metres)
    body = np.concatenate([np.concatenate(v[0]) for v in groups["airframe"].values()])
    info["wingtip_x"] = float(body[:, 0].max())
    tip = body[body[:, 0] > body[:, 0].max() - 0.15]
    info["wingtip"] = tip.mean(0).round(3).tolist()
    # the pilot's eye: under the canopy's top, a little behind the instrument panel
    canopy = np.concatenate([np.concatenate(v[0]) for v in groups["canopy"].values()])
    panel = np.concatenate([np.concatenate(v[0]) for v in groups["cockpit"].values()])
    info["eye"] = [0.0, round(float(canopy[:, 1].max()) - 0.3 * SCALE, 3), round(float(panel[:, 2].max()) + 0.55 * SCALE, 3)]
    info["nose_z"] = float(body[:, 2].min())
    info["tail_z"] = float(body[:, 2].max())
    info["bbox"] = [body.min(0).round(3).tolist(), body.max(0).round(3).tolist()]
    # the right wing's upper surface as a quad (root LE, tip LE, tip TE, root TE), for the vapour
    tail = min(v["pivot"][2] for k, v in info["surfaces"].items() if k.startswith("stab")) - 0.6
    half = body[:, 0].max()
    quad = []
    for x in (0.3 * half, 0.9 * half):
        m = (abs(body[:, 0] - x) < 0.4) & (abs(body[:, 1] - tip[:, 1].mean()) < 1.2) & (body[:, 2] < tail)
        sl = body[m]
        top = sl[:, 1].max()
        quad.append((x, top, sl[:, 2].min(), sl[:, 2].max()))
    (xr, yr, ler, ter), (xt, yt, let, tet) = quad
    info["wing"] = [[xr, yr, ler], [xt, yt, let], [xt, yt, tet], [xr, yr, ter]]

    binname = f"{key}.bin"
    open(os.path.join(OUT, binname), "wb").write(bytes(gl.bin))
    cock = f"cockpit_{cfg['cockpit'].split('_')[0]}.jpg"
    # full textures, and half-size ones for phones (`_low.gltf`)
    for sub, div in (("", 1), ("low/", 2)):
        tdir = os.path.join(OUT, "textures", sub)
        os.makedirs(tdir, exist_ok=True)
        save_tex(os.path.join(PACK, cfg["normal"]), os.path.join(tdir, f"{key}_normal.jpg"), TEX // div, 92)
        spec = Image.open(os.path.join(PACK, cfg["spec"])).convert("L").resize((512 // div, 512 // div), Image.LANCZOS)
        s = np.asarray(spec).astype(np.float32) / 255
        mr = np.stack([np.full_like(s, 1.0), 0.78 - 0.38 * s, 0.15 * s], -1)
        Image.fromarray((mr * 255).astype(np.uint8)).save(os.path.join(tdir, f"{key}_mr.jpg"), quality=90)
        save_tex(os.path.join(PACK, cfg["cockpit"]), os.path.join(tdir, cock), 512 // div, 85)
        for name, file in cfg["liveries"].items():
            save_tex(os.path.join(PACK, file), os.path.join(tdir, f"{key}_{name}.jpg"), TEX // div, 88)
    for name in cfg["liveries"]:
        for sub, suffix in (("", ""), ("low/", "_low")):
            write_gltf(gl, key, name, sub, suffix, binname, cock)
    return info


def write_gltf(gl, key, name, sub, suffix, binname, cock):
    t = f"textures/{sub}"
    images = [f"{t}{key}_{name}.jpg", f"{t}{key}_normal.jpg", f"{t}{key}_mr.jpg",
              "textures/f16_nozzle.jpg", f"{t}{cock}", "textures/hud.png", "textures/glass_inner.png"]
    tx_ = lambda i: {"index": i}
    materials = [
        {"name": "body", "pbrMetallicRoughness": {"baseColorTexture": tx_(0), "metallicRoughnessTexture": tx_(2),
                                                 "metallicFactor": 1.0, "roughnessFactor": 1.0},
         "normalTexture": tx_(1)},
        {"name": "nozzle", "pbrMetallicRoughness": {"baseColorTexture": tx_(3), "metallicFactor": 0.7, "roughnessFactor": 0.45}},
        {"name": "glass", "pbrMetallicRoughness": {"baseColorFactor": [0.42, 0.47, 0.5, 0.55], "metallicFactor": 0.0,
                                                  "roughnessFactor": 0.06}, "alphaMode": "BLEND"},
        {"name": "cockpit", "pbrMetallicRoughness": {"baseColorTexture": tx_(4), "metallicFactor": 0.0, "roughnessFactor": 0.8}},
        {"name": "hud", "pbrMetallicRoughness": {"baseColorTexture": tx_(5), "metallicFactor": 0.0, "roughnessFactor": 1.0},
         "alphaMode": "BLEND", "extensions": {"KHR_materials_unlit": {}}},
        # the canopy seen from inside (first person)
        {"name": "glass_inner", "pbrMetallicRoughness": {"baseColorTexture": tx_(6), "metallicFactor": 0.0, "roughnessFactor": 0.06},
         "alphaMode": "BLEND", "doubleSided": True},
    ]
    # only the materials (and images) this jet uses: unused images would still load
    used = sorted({p["material"] for m in gl.meshes for p in m["primitives"]})
    remap = {old: new for new, old in enumerate(used)}
    mats = [materials[i] for i in used]
    img_used = sorted({v["index"] for m in mats for k, v in _textures(m)})
    imap = {old: new for new, old in enumerate(img_used)}
    for m in mats:
        for k, v in _textures(m):
            v["index"] = imap[v["index"]]
    meshes = [{"name": m["name"], "primitives": [dict(p, material=remap[p["material"]]) for p in m["primitives"]]} for m in gl.meshes]
    doc = {
        "asset": {"version": "2.0", "generator": "flight_sim scripts/jets/build.py (Modern Fighter Jets pack)"},
        "scene": 0,
        "scenes": [{"name": f"{key}_{name}", "nodes": list(range(len(gl.nodes)))}],
        "nodes": gl.nodes,
        "meshes": meshes,
        "materials": mats,
        "textures": [{"source": i, "sampler": 0} for i in range(len(img_used))],
        "samplers": [{"magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497}],
        "images": [{"uri": images[i]} for i in img_used],
        "accessors": gl.accessors,
        "bufferViews": gl.views,
        "buffers": [{"uri": binname, "byteLength": len(gl.bin)}],
    }
    if any("extensions" in m for m in mats):
        doc["extensionsUsed"] = ["KHR_materials_unlit"]
    json.dump(doc, open(os.path.join(OUT, f"{key}_{name}{suffix}.gltf"), "w"), separators=(",", ":"))


def _textures(m):
    """(key, texture info) pairs of a material, as references into it."""
    out = []
    pbr = m.get("pbrMetallicRoughness", {})
    for k in ("baseColorTexture", "metallicRoughnessTexture"):
        if k in pbr:
            out.append((k, pbr[k]))
    if "normalTexture" in m:
        out.append(("normalTexture", m["normalTexture"]))
    return out


os.makedirs(OUT, exist_ok=True)
os.makedirs(os.path.join(OUT, "textures"), exist_ok=True)
save_tex(os.path.join(PACK, "f16nozzle.png"), os.path.join(OUT, "textures", "f16_nozzle.jpg"), 256, 85)
save_tex(os.path.join(PACK, "hud.png"), os.path.join(OUT, "textures", "hud.png"), 256, mode="RGBA")
save_tex(os.path.join(PACK, "aircraftglassint.png"), os.path.join(OUT, "textures", "glass_inner.png"), 128, mode="RGBA")
infos = {k: build(k, c) for k, c in PLANES.items()}
json.dump(infos, open(os.path.join(sys.argv[4] if len(sys.argv) > 4 else OUT, "info.json"), "w"), indent=1)
print(json.dumps(infos, indent=1))
