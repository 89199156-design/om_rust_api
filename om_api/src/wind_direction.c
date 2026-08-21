/*
 * Bulk wind-direction implementation corresponding to Open-Meteo
 * Sources/CHelper/src/shim.c::windirectionFast.
 * Upstream revision: fc670930b55c963b10e9578c8628a824da43a3ab
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */
#define _USE_MATH_DEFINES
#include <math.h>
#include <stddef.h>

void om_windirection_fast(const size_t num_points, const float *ys,
                          const float *xs, float *out) {
  float pi = M_PI;
  float pi_2 = M_PI_2;

  for (size_t i = 0; i < num_points; i++) {
    float y = ys[i];
    float x = xs[i];
    if (x == 0) {
      out[i] = y < 0 ? 90 : 270;
      continue;
    }
    if (y == 0) {
      out[i] = x < 0 ? 360 : 180;
      continue;
    }
    int swap = fabs(x) < fabs(y);
    float atan_input =
        (swap ? y : x) == 0
            ? ((swap ? x : y) / 0.00000001)
            : ((swap ? x : y) / (swap ? y : x));

    float a1 = 0.99997726f;
    float a3 = -0.33262347f;
    float a5 = 0.19354346f;
    float a7 = -0.11643287f;
    float a9 = 0.05265332f;
    float a11 = -0.01172120f;

    float x_sq = atan_input * atan_input;
    float res = atan_input *
                fmaf(x_sq,
                     fmaf(x_sq,
                          fmaf(x_sq,
                               fmaf(x_sq, fmaf(x_sq, a11, a9), a7), a5),
                          a3),
                     a1);

    res = swap ? copysignf(pi_2, atan_input) - res : res;
    if (x < 0.0f) {
      res = copysignf(pi, y) + res;
    }

    out[i] = res * (180 / pi) + 180;
  }
}
