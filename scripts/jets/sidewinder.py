"""Exports one Sidewinder from Samuel's Sidewinders.blend as the game's missile.

usage (the .blend is a bought model: open it untrusted):
  blender -b --factory-startup --disable-autoexec Sidewinders.blend \
      --python scripts/jets/sidewinder.py -- Aim-9M assets/models/jets/aim9m.gltf

Like the jets it belongs in the private assets repo. The output is one mesh,
one primitive: the materials' flat colours baked into COLOR_0, so the game can
keep drawing missiles as a plain `Mesh3d` + one material. Frame: the missile
lies along Y, nose at -Y and nozzle at +Y, centred, 3.66 m long (the length
of the old capsule, so the motor effects at +Y still sit on the nozzle).
"""
import bpy, json, os, struct, sys
import numpy as np

name, out = sys.argv[sys.argv.index("--") + 1:]
LENGTH = 3.66
# ~3k triangles is a lot for a missile: four hang on every jet
DECIMATE = 0.4

o = bpy.data.objects[name]
mod = o.modifiers.new("decimate", "DECIMATE")
mod.ratio = DECIMATE
dg = bpy.context.evaluated_depsgraph_get()
me = o.evaluated_get(dg).to_mesh()
me.calc_loop_triangles()
mw = np.array(o.matrix_world)
cols = []
for m in me.materials:
    b = m.node_tree.nodes.get("Principled BSDF") if m and m.use_nodes else None
    cols.append(tuple(b.inputs["Base Color"].default_value[:3]) if b else (0.6, 0.6, 0.6))

P, N, C = [], [], []
for t in me.loop_triangles:
    c = cols[t.material_index] if t.material_index < len(cols) else (0.6, 0.6, 0.6)
    for k, li in enumerate(t.loops):
        P.append(tuple(me.vertices[t.vertices[k]].co))
        N.append(tuple(me.corner_normals[li].vector))
        C.append(c)
P = np.array(P, np.float64)
P = (np.c_[P, np.ones(len(P))] @ mw.T)[:, :3]
N = np.array(N) @ np.linalg.inv(mw[:3, :3])
N /= np.linalg.norm(N, axis=1, keepdims=True) + 1e-9
C = np.array(C)

# Blender's Z-up axes map straight across (the missile is round: its roll is
# arbitrary); centre it and scale it to LENGTH along Y
lo, hi = P.min(0), P.max(0)
centre = (lo + hi) / 2
scale = LENGTH / (hi[1] - lo[1])
P = (P - centre) * scale

# weld identical corners
key = np.round(np.c_[P, N, C], 5)
uniq, inv = np.unique(key, axis=0, return_inverse=True)
inv = inv.reshape(-1)
pos = uniq[:, 0:3].astype(np.float32)
nrm = uniq[:, 3:6].astype(np.float32)
nrm /= np.linalg.norm(nrm, axis=1, keepdims=True) + 1e-9
col = uniq[:, 6:9].astype(np.float32)
idx = inv.astype(np.uint16 if len(uniq) < 65535 else np.uint32)

blob = bytearray()
views, accessors = [], []


def add(arr, comp, typ, target, minmax=False):
    while len(blob) % 4:
        blob.extend(b"\0")
    views.append({"buffer": 0, "byteOffset": len(blob), "byteLength": arr.nbytes, "target": target})
    blob.extend(arr.tobytes())
    acc = {"bufferView": len(views) - 1, "componentType": comp, "count": len(arr), "type": typ}
    if minmax:
        acc["min"], acc["max"] = arr.min(0).tolist(), arr.max(0).tolist()
    accessors.append(acc)
    return len(accessors) - 1


prim = {
    "attributes": {
        "POSITION": add(pos, 5126, "VEC3", 34962, True),
        "NORMAL": add(nrm, 5126, "VEC3", 34962),
        "COLOR_0": add(col, 5126, "VEC3", 34962),
    },
    "indices": add(idx, 5123 if idx.dtype == np.uint16 else 5125, "SCALAR", 34963),
    "material": 0,
}
stem = os.path.splitext(os.path.basename(out))[0]
doc = {
    "asset": {"version": "2.0", "generator": "flight_sim scripts/jets/sidewinder.py"},
    "scene": 0,
    "scenes": [{"nodes": [0]}],
    "nodes": [{"name": stem, "mesh": 0}],
    "meshes": [{"name": stem, "primitives": [prim]}],
    "materials": [{"name": "missile", "pbrMetallicRoughness": {"metallicFactor": 0.3, "roughnessFactor": 0.45}}],
    "accessors": accessors,
    "bufferViews": views,
    "buffers": [{"uri": stem + ".bin", "byteLength": len(blob)}],
}
open(os.path.join(os.path.dirname(out), stem + ".bin"), "wb").write(bytes(blob))
json.dump(doc, open(out, "w"), separators=(",", ":"))
print(f"EXPORTED {name}: {len(idx) // 3} triangles, {len(pos)} vertices, {hi[1] - lo[1]:.1f} units -> {LENGTH} m")
