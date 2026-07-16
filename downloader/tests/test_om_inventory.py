import struct
import tempfile
import unittest
from pathlib import Path

from om_downloader.om_inventory import infer_pressure_levels_hpa, load_om_inventory


def _pack_array(name):
    encoded_name = name.encode("utf-8")
    return (
        struct.pack("<BBHIQQQff", 20, 1, len(encoded_name), 0, 128, 96, 3, 1.0, 0.0)
        + b"".join(struct.pack("<Q", item) for item in (721, 1440, 385))
        + b"".join(struct.pack("<Q", item) for item in (1, 50, 385))
        + encoded_name
    )


def _pack_root(name, children):
    encoded_name = name.encode("utf-8")
    return (
        struct.pack("<BBHI", 0, 4, len(encoded_name), len(children))
        + b"".join(struct.pack("<Q", size) for _offset, size in children)
        + b"".join(struct.pack("<Q", offset) for offset, _size in children)
        + encoded_name
    )


def _sample_file():
    temp850 = _pack_array("temperature_850hPa")
    temp500 = _pack_array("temperature_500hPa")
    cloud = _pack_array("cloud_cover")
    offsets = [256, 256 + len(temp850) + 16, 256 + len(temp850) + len(temp500) + 32]
    root = _pack_root(
        "root",
        [(offsets[0], len(temp850)), (offsets[1], len(temp500)), (offsets[2], len(cloud))],
    )
    root_offset = offsets[2] + len(cloud) + 16
    blob = bytearray(root_offset + len(root) + 24)
    blob[0:3] = b"OM\x03"
    blob[offsets[0] : offsets[0] + len(temp850)] = temp850
    blob[offsets[1] : offsets[1] + len(temp500)] = temp500
    blob[offsets[2] : offsets[2] + len(cloud)] = cloud
    blob[root_offset : root_offset + len(root)] = root
    blob[-24:] = struct.pack("<2sBBIQQ", b"OM", 3, 0, 0, root_offset, len(root))
    return bytes(blob)


class OmInventoryTests(unittest.TestCase):
    def test_infer_pressure_levels_from_variable_names(self):
        levels = infer_pressure_levels_hpa(["temperature_850hPa", "wind_500hPa", "cloud_cover"])
        self.assertEqual(levels, [850, 500])

    def test_load_om_inventory_reads_arrays_and_pressure_levels(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sample.om"
            path.write_bytes(_sample_file())

            inventory = load_om_inventory(path)

            self.assertEqual(
                sorted(inventory.available_variables),
                ["cloud_cover", "temperature_500hPa", "temperature_850hPa"],
            )
            self.assertEqual(inventory.pressure_levels_hpa, [850, 500])
            self.assertEqual(inventory.arrays["temperature_850hPa"].dimensions, (721, 1440, 385))
            self.assertEqual(inventory.arrays["temperature_850hPa"].chunks, (1, 50, 385))
            self.assertEqual(inventory.arrays["temperature_850hPa"].lut_offset, 96)
            self.assertEqual(inventory.arrays["temperature_850hPa"].lut_size, 128)


if __name__ == "__main__":
    unittest.main()
