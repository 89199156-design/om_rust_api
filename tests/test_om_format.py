import struct
import unittest

from om_downloader.om_format import collect_array_variables, parse_om_file, parse_om_trailer


OM_HEADER = b"OM\x03"
OM_TRAILER_MAGIC = b"OM"


def _pack_array(
    name,
    *,
    dimensions,
    chunks,
    lut_offset,
    lut_size,
    scale_factor=1.0,
    add_offset=0.0,
    children=(),
):
    encoded_name = name.encode("utf-8")
    header = struct.pack(
        "<BBHIQQQff",
        20,
        1,
        len(encoded_name),
        len(children),
        lut_size,
        lut_offset,
        len(dimensions),
        scale_factor,
        add_offset,
    )
    return (
        header
        + b"".join(struct.pack("<Q", size) for _offset, size in children)
        + b"".join(struct.pack("<Q", offset) for offset, _size in children)
        + b"".join(struct.pack("<Q", item) for item in dimensions)
        + b"".join(struct.pack("<Q", item) for item in chunks)
        + encoded_name
    )


def _pack_string_scalar(name, value):
    encoded_name = name.encode("utf-8")
    encoded_value = value.encode("utf-8")
    header = struct.pack("<BBHI", 11, 4, len(encoded_name), 0)
    return header + struct.pack("<Q", len(encoded_value)) + encoded_value + encoded_name


def _pack_root(name, children):
    encoded_name = name.encode("utf-8")
    header = struct.pack("<BBHI", 0, 4, len(encoded_name), len(children))
    child_sizes = b"".join(struct.pack("<Q", size) for _offset, size in children)
    child_offsets = b"".join(struct.pack("<Q", offset) for offset, _size in children)
    return header + child_sizes + child_offsets + encoded_name


def _sample_om_file():
    temperature = _pack_array(
        "temperature_2m",
        dimensions=[721, 1440, 385],
        chunks=[1, 50, 385],
        lut_offset=96,
        lut_size=128,
        scale_factor=10.0,
        add_offset=0.0,
    )
    humidity = _pack_array(
        "relative_humidity_2m",
        dimensions=[721, 1440, 385],
        chunks=[1, 50, 385],
        lut_offset=224,
        lut_size=128,
        scale_factor=1.0,
        add_offset=0.0,
    )
    temp_offset = 256
    humidity_offset = temp_offset + len(temperature) + 16
    root = _pack_root("root", [(temp_offset, len(temperature)), (humidity_offset, len(humidity))])
    root_offset = humidity_offset + len(humidity) + 16

    blob = bytearray(root_offset + len(root) + 24)
    blob[0:3] = OM_HEADER
    blob[temp_offset : temp_offset + len(temperature)] = temperature
    blob[humidity_offset : humidity_offset + len(humidity)] = humidity
    blob[root_offset : root_offset + len(root)] = root
    blob[-24:] = struct.pack("<2sBBIQQ", OM_TRAILER_MAGIC, 3, 0, 0, root_offset, len(root))
    return bytes(blob)


def _sample_om_file_with_scalar_metadata():
    unit = _pack_string_scalar("unit", "°C")
    unit_offset = 256
    temperature_offset = unit_offset + len(unit) + 16
    temperature = _pack_array(
        "temperature_2m",
        dimensions=[721, 1440, 385],
        chunks=[1, 50, 385],
        lut_offset=96,
        lut_size=128,
        scale_factor=10.0,
        children=[(unit_offset, len(unit))],
    )
    root = _pack_root("root", [(temperature_offset, len(temperature))])
    root_offset = temperature_offset + len(temperature) + 16

    blob = bytearray(root_offset + len(root) + 24)
    blob[0:3] = OM_HEADER
    blob[unit_offset : unit_offset + len(unit)] = unit
    blob[temperature_offset : temperature_offset + len(temperature)] = temperature
    blob[root_offset : root_offset + len(root)] = root
    blob[-24:] = struct.pack("<2sBBIQQ", OM_TRAILER_MAGIC, 3, 0, 0, root_offset, len(root))
    return bytes(blob)


class OmFormatTests(unittest.TestCase):
    def test_parse_trailer_reads_root_location(self):
        data = _sample_om_file()
        trailer = parse_om_trailer(data[-24:])
        self.assertEqual(trailer.version, 3)
        self.assertGreater(trailer.root_offset, 0)
        self.assertGreater(trailer.root_size, 0)

    def test_parse_om_file_reads_array_children(self):
        root = parse_om_file(_sample_om_file())
        self.assertEqual(root.name, "root")
        self.assertEqual(sorted(root.children), ["relative_humidity_2m", "temperature_2m"])

        temperature = root.children["temperature_2m"]
        self.assertTrue(temperature.is_array)
        self.assertEqual(temperature.dimensions, (721, 1440, 385))
        self.assertEqual(temperature.chunks, (1, 50, 385))
        self.assertEqual(temperature.lut_offset, 96)
        self.assertEqual(temperature.lut_size, 128)
        self.assertEqual(temperature.scale_factor, 10.0)

    def test_collect_array_variables_returns_inventory(self):
        root = parse_om_file(_sample_om_file())
        inventory = collect_array_variables(root)
        self.assertEqual(sorted(inventory), ["relative_humidity_2m", "temperature_2m"])
        self.assertEqual(inventory["temperature_2m"]["dimensions"], [721, 1440, 385])
        self.assertEqual(inventory["temperature_2m"]["chunks"], [1, 50, 385])
        self.assertEqual(inventory["temperature_2m"]["lut_offset"], 96)
        self.assertEqual(inventory["temperature_2m"]["lut_size"], 128)

    def test_parse_om_file_reads_string_scalar_metadata_child(self):
        root = parse_om_file(_sample_om_file_with_scalar_metadata())

        temperature = root.children["temperature_2m"]
        self.assertEqual(sorted(temperature.children), ["unit"])
        self.assertEqual(temperature.children["unit"].name, "unit")
        self.assertEqual(temperature.children["unit"].scalar_value, "°C")


if __name__ == "__main__":
    unittest.main()
