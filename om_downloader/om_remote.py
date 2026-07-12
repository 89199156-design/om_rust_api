from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import Iterable, Protocol

from .http_range import HttpObjectInfo, fetch_byte_range, probe_http_object
from .om_format import (
    OM_HEADER_MAGIC,
    OM_TRAILER_SIZE,
    OmVariable,
    parse_om_trailer,
    parse_om_variable_blob,
)
from .om_inventory import OmArrayInfo, OmInventory, infer_pressure_levels_hpa, inventory_from_root


class ByteRangeSource(Protocol):
    def content_length(self) -> int:
        ...

    def read_range(self, start: int, end: int) -> bytes:
        ...


@dataclass
class HttpByteRangeSource:
    url: str
    timeout: int = 30
    _object_info: HttpObjectInfo | None = field(default=None, init=False, repr=False)

    def _info(self) -> HttpObjectInfo:
        if self._object_info is None:
            self._object_info = probe_http_object(self.url, timeout=self.timeout)
            if not self._object_info.accept_ranges:
                raise ValueError("remote object does not advertise byte range support")
        return self._object_info

    def content_length(self) -> int:
        return self._info().content_length

    def read_range(self, start: int, end: int) -> bytes:
        content_length = self.content_length()
        if start < 0 or end <= start:
            raise ValueError("byte range end must be greater than start")
        if end > content_length:
            raise ValueError("byte range exceeds remote content length")
        return fetch_byte_range(self.url, start, end, timeout=self.timeout)


def _read_exact(source: ByteRangeSource, start: int, end: int) -> bytes:
    payload = source.read_range(start, end)
    expected = end - start
    if len(payload) != expected:
        raise ValueError(f"byte range source returned {len(payload)} bytes, expected {expected}")
    return payload


def _read_variable_node(
    source: ByteRangeSource,
    offset: int,
    size: int,
    content_length: int,
) -> OmVariable:
    if offset < 0 or size <= 0 or offset + size > content_length:
        raise ValueError("OM variable exceeds remote object bounds")
    payload = _read_exact(source, offset, offset + size)
    return parse_om_variable_blob(payload, offset, size)


def _read_variable_tree(
    source: ByteRangeSource,
    offset: int,
    size: int,
    content_length: int,
) -> OmVariable:
    variable = _read_variable_node(source, offset, size, content_length)
    for child_reference in variable.child_references:
        child = _read_variable_tree(
            source,
            child_reference.offset,
            child_reference.size,
            content_length,
        )
        variable.children[child.name] = child
    return variable


def _read_remote_om_root_node(
    source: ByteRangeSource,
    *,
    header_probe_size: int = 40,
) -> tuple[OmVariable, int]:
    content_length = source.content_length()
    if content_length < 3 + OM_TRAILER_SIZE:
        raise ValueError("OM file is too small")

    header_end = min(max(header_probe_size, 3), content_length)
    header = _read_exact(source, 0, header_end)
    if header[0:2] != OM_HEADER_MAGIC:
        raise ValueError("invalid OM header magic")
    if header[2] != 3:
        raise ValueError(f"unsupported OM version: {header[2]}")

    trailer_bytes = _read_exact(source, content_length - OM_TRAILER_SIZE, content_length)
    trailer = parse_om_trailer(trailer_bytes)
    return _read_variable_node(source, trailer.root_offset, trailer.root_size, content_length), content_length


def read_remote_om_root(source: ByteRangeSource, *, header_probe_size: int = 40) -> OmVariable:
    root, content_length = _read_remote_om_root_node(source, header_probe_size=header_probe_size)
    for child_reference in root.child_references:
        child = _read_variable_tree(
            source,
            child_reference.offset,
            child_reference.size,
            content_length,
        )
        root.children[child.name] = child
    return root


def load_remote_om_inventory(source: ByteRangeSource) -> OmInventory:
    return inventory_from_root(read_remote_om_root(source))


def _array_info_from_variable(variable: OmVariable, path: str) -> OmArrayInfo:
    return OmArrayInfo(
        name=variable.name,
        path=path,
        data_type=variable.data_type,
        compression=variable.compression,
        dimensions=tuple(variable.dimensions),
        chunks=tuple(variable.chunks),
        lut_offset=variable.lut_offset,
        lut_size=variable.lut_size,
        scale_factor=variable.scale_factor,
        add_offset=variable.add_offset,
    )


def _read_child_nodes(
    source: ByteRangeSource,
    references: Iterable,
    content_length: int,
    *,
    metadata_workers: int,
) -> list[OmVariable]:
    refs = list(references)
    if not refs:
        return []
    if metadata_workers <= 1 or len(refs) == 1:
        return [
            _read_variable_node(source, item.offset, item.size, content_length)
            for item in refs
        ]
    with ThreadPoolExecutor(max_workers=min(metadata_workers, len(refs))) as executor:
        return list(
            executor.map(
                lambda item: _read_variable_node(source, item.offset, item.size, content_length),
                refs,
            )
        )


def load_remote_om_inventory_fast(
    source: ByteRangeSource,
    wanted_variables: Iterable[str],
    *,
    metadata_workers: int = 4,
) -> OmInventory:
    wanted = set(wanted_variables)
    root, content_length = _read_remote_om_root_node(source)
    arrays: dict[str, OmArrayInfo] = {}

    def walk_container(node: OmVariable, prefix: str) -> None:
        for child in _read_child_nodes(
            source,
            node.child_references,
            content_length,
            metadata_workers=metadata_workers,
        ):
            path = f"{prefix}/{child.name}" if prefix else child.name
            if child.is_array:
                if child.name in wanted:
                    arrays[child.name] = _array_info_from_variable(child, path)
                continue
            if child.child_references:
                walk_container(child, path)

    walk_container(root, root.name)
    return OmInventory(
        arrays=arrays,
        pressure_levels_hpa=infer_pressure_levels_hpa(tuple(arrays)),
    )
