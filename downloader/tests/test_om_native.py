import unittest
from unittest.mock import patch

from om_downloader.om_native import TurboPForDecoder, load_default_turbopfor_decoder


class _FakeP4nddec64:
    def __init__(self, values, consumed):
        self.values = values
        self.consumed = consumed
        self.argtypes = None
        self.restype = None
        self.calls = []

    def __call__(self, in_buffer, count, out_buffer):
        count_value = int(count.value if hasattr(count, "value") else count)
        self.calls.append((bytes(in_buffer[:4]), count_value))
        for idx, value in enumerate(self.values[:count_value]):
            out_buffer[idx] = value
        return self.consumed


class _FakeLibrary:
    def __init__(self, values, consumed):
        self.p4nddec64 = _FakeP4nddec64(values, consumed)


class OmNativeTests(unittest.TestCase):
    def test_turbopfor_decoder_calls_p4nddec64(self):
        library = _FakeLibrary([100, 125, 180, 210], consumed=7)
        decoder = TurboPForDecoder(library)

        decoded, consumed = decoder.decode_u64_delta0(b"abcdefghi", count=4)

        self.assertEqual(decoded, [100, 125, 180, 210])
        self.assertEqual(consumed, 7)
        self.assertEqual(library.p4nddec64.calls, [(b"abcd", 4)])

    def test_turbopfor_decoder_rejects_invalid_consumed_length(self):
        decoder = TurboPForDecoder(_FakeLibrary([1, 2], consumed=99))
        with self.assertRaises(ValueError) as ctx:
            decoder.decode_u64_delta0(b"abc", count=2)
        self.assertIn("invalid compressed byte count", str(ctx.exception))

    def test_default_loader_requires_library_path_or_findable_library(self):
        with patch.dict("os.environ", {}, clear=True), patch("ctypes.util.find_library", return_value=None):
            with self.assertRaises(NotImplementedError) as ctx:
                load_default_turbopfor_decoder()
        self.assertIn("OM_TURBOPFOR_LIB", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
