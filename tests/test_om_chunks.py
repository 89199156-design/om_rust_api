import unittest

from om_downloader.om_chunks import chunk_index_ranges_for_selection, chunk_indices_for_selection


class OmChunkTests(unittest.TestCase):
    def test_chunk_indices_for_regular_y_x_time_selection(self):
        indices = chunk_indices_for_selection(
            dimensions=(721, 1440, 385),
            chunks=(1, 50, 385),
            selection_ranges=((352, 354), (272, 569), (0, 385)),
        )
        self.assertEqual(indices, [352 * 29 + 5, 352 * 29 + 6, 352 * 29 + 7, 352 * 29 + 8, 352 * 29 + 9, 352 * 29 + 10, 352 * 29 + 11, 353 * 29 + 5, 353 * 29 + 6, 353 * 29 + 7, 353 * 29 + 8, 353 * 29 + 9, 353 * 29 + 10, 353 * 29 + 11])

    def test_chunk_index_ranges_groups_contiguous_indices(self):
        ranges = chunk_index_ranges_for_selection(
            dimensions=(721, 1440, 385),
            chunks=(1, 50, 385),
            selection_ranges=((352, 354), (272, 569), (0, 385)),
        )
        self.assertEqual(ranges, [(10213, 10220), (10242, 10249)])

    def test_selection_ranges_must_match_dimensions(self):
        with self.assertRaises(ValueError) as ctx:
            chunk_indices_for_selection(
                dimensions=(721, 1440, 385),
                chunks=(1, 50, 385),
                selection_ranges=((0, 1), (0, 1)),
            )
        self.assertIn("same length", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
