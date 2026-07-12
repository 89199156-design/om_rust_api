import struct
import unittest

from om_downloader.om_inventory import OmArrayInfo
from om_downloader.om_remote_ranges import plan_remote_array_data_byte_ranges


class _FakeByteRangeSource:
    def __init__(self, data):
        self.data = data
        self.requests = []

    def read_range(self, start, end):
        self.requests.append((start, end))
        return self.data[start:end]


class _FakeDecoder:
    def __init__(self, values):
        self.values = values
        self.payloads = []

    def decode_u64_delta0(self, payload, *, count):
        self.payloads.append(payload)
        return self.values[:count], len(payload)


def _array_info(*, lut_offset=100, lut_size=40):
    return OmArrayInfo(
        name="temperature_2m",
        path="root/temperature_2m",
        data_type=20,
        compression=1,
        dimensions=(4, 1),
        chunks=(1, 1),
        lut_offset=lut_offset,
        lut_size=lut_size,
        scale_factor=1.0,
        add_offset=0.0,
    )


def _array_info_grid(*, lut_offset=100, lut_size=136):
    return OmArrayInfo(
        name="temperature_2m",
        path="root/temperature_2m",
        data_type=20,
        compression=1,
        dimensions=(4, 4),
        chunks=(1, 1),
        lut_offset=lut_offset,
        lut_size=lut_size,
        scale_factor=1.0,
        add_offset=0.0,
    )


def _array_info_long(*, lut_offset=100, lut_size=1536):
    return OmArrayInfo(
        name="temperature_2m",
        path="root/temperature_2m",
        data_type=20,
        compression=1,
        dimensions=(130, 1),
        chunks=(1, 1),
        lut_offset=lut_offset,
        lut_size=lut_size,
        scale_factor=1.0,
        add_offset=0.0,
    )


class OmRemoteRangeTests(unittest.TestCase):
    def test_plan_remote_array_data_byte_ranges_reads_lut_and_returns_data_ranges(self):
        lut_payload = b"".join(struct.pack("<Q", item) for item in [2000, 2020, 2060, 2100, 2140])
        data = bytearray(200)
        data[100 : 100 + len(lut_payload)] = lut_payload
        source = _FakeByteRangeSource(bytes(data))

        plan = plan_remote_array_data_byte_ranges(
            source,
            _array_info(),
            selection_ranges=((1, 3), (0, 1)),
            lut_codec="plain",
        )

        self.assertEqual(source.requests, [(100, 140)])
        self.assertEqual(plan.lut_byte_ranges, [(100, 140)])
        self.assertEqual(plan.data_byte_ranges, [(2020, 2100)])
        self.assertEqual(plan.lut_bytes_read, 40)

    def test_plan_remote_array_data_byte_ranges_uses_turbopfor_decoder_by_default(self):
        data = bytearray(200)
        data[100:140] = b"compressed".ljust(40, b"\0")
        source = _FakeByteRangeSource(bytes(data))
        decoder = _FakeDecoder([2000, 2020, 2060, 2100, 2140])

        plan = plan_remote_array_data_byte_ranges(
            source,
            _array_info(),
            selection_ranges=((1, 3), (0, 1)),
            lut_decoder=decoder,
        )

        self.assertEqual(decoder.payloads, [b"compressed".ljust(40, b"\0")])
        self.assertEqual(plan.data_byte_ranges, [(2020, 2100)])

    def test_plan_remote_array_data_byte_ranges_merges_small_default_gaps(self):
        offsets = [
            1000,
            1020,
            20000,
            40000,
            61000,
            61020,
            80000,
            100000,
            120000,
            120020,
            140000,
            160000,
            180000,
            180020,
            200000,
            220000,
            240000,
        ]
        lut_payload = b"".join(struct.pack("<Q", item) for item in offsets)
        data = bytearray(100 + len(lut_payload))
        data[100 : 100 + len(lut_payload)] = lut_payload
        source = _FakeByteRangeSource(bytes(data))

        default_plan = plan_remote_array_data_byte_ranges(
            source,
            _array_info_grid(),
            selection_ranges=((0, 4), (0, 1)),
            lut_codec="plain",
        )
        conservative_plan = plan_remote_array_data_byte_ranges(
            source,
            _array_info_grid(),
            selection_ranges=((0, 4), (0, 1)),
            lut_codec="plain",
            io_size_merge=512,
        )

        self.assertEqual(default_plan.data_byte_ranges, [(1000, 180020)])
        self.assertEqual(
            conservative_plan.data_byte_ranges,
            [(1000, 1020), (61000, 61020), (120000, 120020), (180000, 180020)],
        )

    def test_plan_remote_array_data_byte_ranges_reads_merged_lut_chunks_once(self):
        offsets = list(range(1000, 1131))
        lut_chunk_payloads = []
        for chunk_start in (0, 64, 128):
            chunk_values = offsets[chunk_start : chunk_start + 64]
            payload = b"".join(struct.pack("<Q", item) for item in chunk_values)
            lut_chunk_payloads.append(payload.ljust(512, b"\0"))
        data = bytearray(100 + 512 * 3)
        data[100 : 100 + 512 * 3] = b"".join(lut_chunk_payloads)
        source = _FakeByteRangeSource(bytes(data))

        plan = plan_remote_array_data_byte_ranges(
            source,
            _array_info_long(lut_offset=100, lut_size=512 * 3),
            selection_ranges=((0, 130), (0, 1)),
            lut_codec="plain",
        )

        self.assertEqual(source.requests, [(100, 1636)])
        self.assertEqual(plan.lut_byte_ranges, [(100, 1636)])
        self.assertEqual(plan.data_byte_ranges, [(1000, 1130)])


if __name__ == "__main__":
    unittest.main()
