import struct
import unittest
from unittest.mock import patch

from om_downloader.om_lut import (
    LUT_CHUNK_COUNT,
    calculate_lut_chunk_length,
    chunk_index_ranges_to_lut_byte_ranges,
    data_byte_ranges_from_lut_offsets,
    decode_plain_u64_lut,
    decode_turbopfor_u64_lut,
)


class _FakeDecoder:
    def decode_u64_delta0(self, payload, *, count):
        if payload != b"compressed":
            raise AssertionError("unexpected payload")
        return [100, 125, 180, 210][:count], 7


class OmLutTests(unittest.TestCase):
    def test_calculate_lut_chunk_length_uses_number_of_chunks_plus_one(self):
        self.assertEqual(calculate_lut_chunk_length(number_of_chunks=128, lut_size=300), 100)
        self.assertEqual(calculate_lut_chunk_length(number_of_chunks=127, lut_size=300), 150)
        self.assertEqual(LUT_CHUNK_COUNT, 64)

    def test_chunk_index_ranges_to_lut_byte_ranges_include_next_offset(self):
        ranges = chunk_index_ranges_to_lut_byte_ranges(
            chunk_index_ranges=[(63, 65), (130, 132)],
            lut_offset=10_000,
            lut_chunk_length=100,
        )
        self.assertEqual(ranges, [(10_000, 10_300)])

    def test_decode_plain_u64_lut(self):
        payload = b"".join(struct.pack("<Q", value) for value in [100, 125, 180, 210])
        self.assertEqual(decode_plain_u64_lut(payload, count=4), [100, 125, 180, 210])

    def test_data_byte_ranges_from_lut_offsets_merges_contiguous_chunks(self):
        ranges = data_byte_ranges_from_lut_offsets(
            lut_offsets=[100, 125, 180, 210, 220],
            chunk_index_ranges=[(0, 2), (2, 4)],
            io_size_merge=0,
        )
        self.assertEqual(ranges, [(100, 220)])

    def test_data_byte_ranges_from_lut_offsets_merges_small_gap(self):
        ranges = data_byte_ranges_from_lut_offsets(
            lut_offsets=[100, 125, 180, 210],
            chunk_index_ranges=[(0, 1), (2, 3)],
            io_size_merge=60,
        )
        self.assertEqual(ranges, [(100, 210)])

    def test_decode_turbopfor_lut_requires_native_decoder(self):
        with patch.dict("os.environ", {}, clear=True), patch("ctypes.util.find_library", return_value=None):
            with self.assertRaises(NotImplementedError) as ctx:
                decode_turbopfor_u64_lut(b"compressed", count=4)
        self.assertIn("native TurboPFor", str(ctx.exception))

    def test_decode_turbopfor_lut_uses_explicit_decoder(self):
        self.assertEqual(
            decode_turbopfor_u64_lut(b"compressed", count=4, decoder=_FakeDecoder()),
            [100, 125, 180, 210],
        )


if __name__ == "__main__":
    unittest.main()
