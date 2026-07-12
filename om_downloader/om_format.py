from __future__ import annotations

from dataclasses import dataclass, field
import struct
from typing import Any


OM_HEADER_MAGIC = b"OM"
OM_TRAILER_SIZE = 24
OM_TRAILER = struct.Struct("<2sBBIQQ")
OM_VARIABLE_HEADER = struct.Struct("<BBHI")
OM_ARRAY_HEADER = struct.Struct("<BBHIQQQff")

ARRAY_DATA_TYPES = set(range(12, 22))


@dataclass(frozen=True)
class OmTrailer:
    version: int
    root_offset: int
    root_size: int


@dataclass(frozen=True)
class OmChildReference:
    offset: int
    size: int


@dataclass
class OmVariable:
    name: str
    data_type: int
    compression: int
    offset: int
    size: int
    dimensions: tuple[int, ...] = ()
    chunks: tuple[int, ...] = ()
    lut_offset: int | None = None
    lut_size: int | None = None
    scale_factor: float | None = None
    add_offset: float | None = None
    scalar_value: int | float | str | None = None
    child_references: tuple[OmChildReference, ...] = ()
    children: dict[str, "OmVariable"] = field(default_factory=dict)

    @property
    def is_array(self) -> bool:
        return self.data_type in ARRAY_DATA_TYPES


def parse_om_trailer(data: bytes) -> OmTrailer:
    if len(data) != OM_TRAILER_SIZE:
        raise ValueError(f"OM trailer must be {OM_TRAILER_SIZE} bytes")
    magic, version, _reserved, _reserved2, root_offset, root_size = OM_TRAILER.unpack(data)
    if magic != OM_HEADER_MAGIC:
        raise ValueError("invalid OM trailer magic")
    if version != 3:
        raise ValueError(f"unsupported OM version: {version}")
    return OmTrailer(version=version, root_offset=root_offset, root_size=root_size)


def _read_u64_sequence(data: bytes, offset: int, count: int) -> tuple[int, ...]:
    end = offset + count * 8
    if end > len(data):
        raise ValueError("OM metadata sequence exceeds file bounds")
    return struct.unpack("<" + "Q" * count, data[offset:end])


def _read_scalar_payload(data: bytes, offset: int, data_type: int, variable_size: int) -> tuple[int, int | float | str | None]:
    if data_type == 0:
        return 0, None

    scalar_formats = {
        1: ("<b", 1),
        2: ("<B", 1),
        3: ("<h", 2),
        4: ("<H", 2),
        5: ("<i", 4),
        6: ("<I", 4),
        7: ("<q", 8),
        8: ("<Q", 8),
        9: ("<f", 4),
        10: ("<d", 8),
    }
    if data_type in scalar_formats:
        fmt, size = scalar_formats[data_type]
        end = offset + size
        if end > variable_size:
            raise ValueError("OM scalar value exceeds variable bounds")
        return size, struct.unpack_from(fmt, data, offset)[0]

    if data_type == 11:
        length_end = offset + 8
        if length_end > variable_size:
            raise ValueError("OM string scalar length exceeds variable bounds")
        string_size = struct.unpack_from("<Q", data, offset)[0]
        value_start = length_end
        value_end = value_start + string_size
        if value_end > variable_size:
            raise ValueError("OM string scalar value exceeds variable bounds")
        return 8 + int(string_size), data[value_start:value_end].decode("utf-8")

    raise ValueError(f"unsupported OM scalar data type: {data_type}")


def parse_om_variable_blob(data: bytes, absolute_offset: int, size: int | None = None) -> OmVariable:
    variable_size = len(data) if size is None else size
    if absolute_offset < 0 or variable_size < OM_VARIABLE_HEADER.size or variable_size > len(data):
        raise ValueError("OM variable exceeds file bounds")
    data_type, compression, name_size, child_count = OM_VARIABLE_HEADER.unpack_from(data, 0)

    if data_type in ARRAY_DATA_TYPES:
        (
            _data_type,
            _compression,
            _name_size,
            _child_count,
            lut_size,
            lut_offset,
            dimension_count,
            scale_factor,
            add_offset,
        ) = OM_ARRAY_HEADER.unpack_from(data, 0)
        cursor = OM_ARRAY_HEADER.size
        child_sizes = _read_u64_sequence(data, cursor, child_count)
        cursor += child_count * 8
        child_offsets = _read_u64_sequence(data, cursor, child_count)
        cursor += child_count * 8
        dimensions = _read_u64_sequence(data, cursor, dimension_count)
        cursor += dimension_count * 8
        chunks = _read_u64_sequence(data, cursor, dimension_count)
        cursor += dimension_count * 8
    else:
        cursor = OM_VARIABLE_HEADER.size
        child_sizes = _read_u64_sequence(data, cursor, child_count)
        cursor += child_count * 8
        child_offsets = _read_u64_sequence(data, cursor, child_count)
        cursor += child_count * 8
        scalar_size, scalar_value = _read_scalar_payload(data, cursor, data_type, variable_size)
        cursor += scalar_size
        dimensions = ()
        chunks = ()
        lut_offset = None
        lut_size = None
        scale_factor = None
        add_offset = None
    if data_type in ARRAY_DATA_TYPES:
        scalar_value = None

    name_start = cursor
    name_end = name_start + name_size
    if name_end > variable_size:
        raise ValueError("OM variable name exceeds variable bounds")
    name = data[name_start:name_end].decode("utf-8")
    child_references = tuple(
        OmChildReference(offset=int(child_offset), size=int(child_size))
        for child_offset, child_size in zip(child_offsets, child_sizes)
    )

    return OmVariable(
        name=name,
        data_type=data_type,
        compression=compression,
        offset=absolute_offset,
        size=variable_size,
        dimensions=tuple(int(item) for item in dimensions),
        chunks=tuple(int(item) for item in chunks),
        lut_offset=int(lut_offset) if lut_offset is not None else None,
        lut_size=int(lut_size) if lut_size is not None else None,
        scale_factor=float(scale_factor) if scale_factor is not None else None,
        add_offset=float(add_offset) if add_offset is not None else None,
        scalar_value=scalar_value,
        child_references=child_references,
    )


def _parse_variable(data: bytes, offset: int, size: int) -> OmVariable:
    if offset < 0 or size < OM_VARIABLE_HEADER.size or offset + size > len(data):
        raise ValueError("OM variable exceeds file bounds")
    variable = parse_om_variable_blob(data[offset : offset + size], offset, size)
    for child_reference in variable.child_references:
        child = _parse_variable(data, child_reference.offset, child_reference.size)
        variable.children[child.name] = child
    return variable


def parse_om_file(data: bytes) -> OmVariable:
    if len(data) < 3 + OM_TRAILER_SIZE:
        raise ValueError("OM file is too small")
    if data[0:2] != OM_HEADER_MAGIC:
        raise ValueError("invalid OM header magic")
    if data[2] != 3:
        raise ValueError(f"unsupported OM version: {data[2]}")
    trailer = parse_om_trailer(data[-OM_TRAILER_SIZE:])
    return _parse_variable(data, trailer.root_offset, trailer.root_size)


def collect_array_variables(root: OmVariable) -> dict[str, dict[str, Any]]:
    inventory: dict[str, dict[str, Any]] = {}

    def walk(node: OmVariable, prefix: str = "") -> None:
        path = f"{prefix}/{node.name}" if prefix else node.name
        if node.is_array:
            inventory[node.name] = {
                "path": path,
                "data_type": node.data_type,
                "compression": node.compression,
                "dimensions": list(node.dimensions),
                "chunks": list(node.chunks),
                "lut_offset": node.lut_offset,
                "lut_size": node.lut_size,
                "scale_factor": node.scale_factor,
                "add_offset": node.add_offset,
            }
        for child in node.children.values():
            walk(child, path)

    walk(root)
    return inventory
