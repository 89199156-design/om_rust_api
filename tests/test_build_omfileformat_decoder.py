import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
BUILD_SCRIPT = REPOSITORY_ROOT / "scripts" / "build_omfileformat_decoder.sh"
PINNED_REVISION = "71f422b2706d8a81f1cecf52ae3073990de1ddbe"
REQUIRED_SYMBOLS = (
    "om_variable_init",
    "om_decoder_init",
    "om_decoder_init_index_read",
    "om_decoder_next_index_read",
    "om_decoder_init_data_read",
    "om_decoder_next_data_read",
    "om_decoder_read_buffer_size",
    "om_decoder_decode_chunks",
    "om_error_string",
    "om_encoder_init",
    "om_encoder_count_chunks",
    "om_encoder_count_chunks_in_array",
    "om_encoder_chunk_buffer_size",
    "om_encoder_compressed_chunk_buffer_size",
    "om_encoder_compress_chunk",
    "om_encoder_lut_buffer_size",
    "om_encoder_compress_lut",
    "om_header_write_size",
    "om_header_write",
    "om_trailer_size",
    "om_trailer_write",
    "om_variable_write_numeric_array_size",
    "om_variable_write_numeric_array",
)
POSIX_BUILD_TESTS_AVAILABLE = (
    os.name == "posix"
    and shutil.which("bash") is not None
    and shutil.which("cc") is not None
    and (
        shutil.which("nm") is not None
        or shutil.which("readelf") is not None
    )
)


class BuildOmFileFormatCodecTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.content = BUILD_SCRIPT.read_text(encoding="utf-8")

    def test_default_source_is_pinned_and_every_runtime_symbol_is_checked(self) -> None:
        self.assertIn(PINNED_REVISION, self.content)
        self.assertIn(
            "https://github.com/open-meteo/om-file-format.git",
            self.content,
        )
        self.assertIn(
            "git@github.com:open-meteo/om-file-format.git",
            self.content,
        )
        self.assertIn("remote set-url origin", self.content)
        for symbol in REQUIRED_SYMBOLS:
            with self.subTest(symbol=symbol):
                self.assertIn(symbol, self.content)

    @unittest.skipUnless(
        POSIX_BUILD_TESTS_AVAILABLE,
        "requires bash, cc, and nm or readelf",
    )
    def test_local_source_build_is_verified_and_published_with_provenance(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            test_root = Path(temporary_directory)
            source_root = test_root / "om-file-format"
            (source_root / "c" / "include").mkdir(parents=True)
            source_directory = source_root / "c" / "src"
            source_directory.mkdir(parents=True)
            self._write_symbol_source(source_directory / "codec.c", REQUIRED_SYMBOLS)
            output_directory = test_root / "native"

            environment = os.environ.copy()
            environment["OM_FILE_FORMAT_SRC"] = str(source_root)
            result = subprocess.run(
                ["bash", str(BUILD_SCRIPT), str(output_directory)],
                cwd=REPOSITORY_ROOT,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(
                result.returncode,
                0,
                msg=f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            library = output_directory / "libomfileformat.so"
            self.assertTrue(library.is_file())
            self.assertGreater(library.stat().st_size, 0)
            self.assertEqual(
                (
                    output_directory / "om-file-format.source-revision"
                ).read_text(encoding="utf-8"),
                "unversioned-local-source\n",
            )
            verification = subprocess.run(
                ["bash", str(BUILD_SCRIPT), "--verify", str(library)],
                cwd=REPOSITORY_ROOT,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(
                verification.returncode,
                0,
                msg=(
                    f"stdout:\n{verification.stdout}"
                    f"stderr:\n{verification.stderr}"
                ),
            )

    @unittest.skipUnless(
        POSIX_BUILD_TESTS_AVAILABLE,
        "requires bash, cc, and nm or readelf",
    )
    def test_verifier_rejects_a_library_without_the_encoder_abi(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            test_root = Path(temporary_directory)
            source = test_root / "decoder-only.c"
            library = test_root / "libdecoder-only.so"
            encoder_start = REQUIRED_SYMBOLS.index("om_encoder_init")
            self._write_symbol_source(source, REQUIRED_SYMBOLS[:encoder_start])
            compile_result = subprocess.run(
                ["cc", "-shared", "-fPIC", str(source), "-o", str(library)],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(
                compile_result.returncode,
                0,
                msg=compile_result.stderr,
            )

            verification = subprocess.run(
                ["bash", str(BUILD_SCRIPT), "--verify", str(library)],
                cwd=REPOSITORY_ROOT,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(verification.returncode, 1)
            self.assertIn(
                "native om-file-format ABI is missing required symbol: om_encoder_init",
                verification.stderr,
            )

    @staticmethod
    def _write_symbol_source(path: Path, symbols: tuple[str, ...]) -> None:
        path.write_text(
            "\n".join(f"void {symbol}(void) {{}}" for symbol in symbols) + "\n",
            encoding="utf-8",
        )


if __name__ == "__main__":
    unittest.main()
