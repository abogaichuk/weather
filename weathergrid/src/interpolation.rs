use crate::winds::Weather;

/// One interpolation axis: the query position `at`, bracketed by samples at
/// `lo` and `hi`. Collapsing the loose `(x, x1, x2)` triples into this value
/// object is what lets the multi-linear helpers below stay under a handful of
/// arguments (audit finding F5).
#[derive(Clone, Copy, Debug)]
pub struct Axis {
    pub at: f64,
    pub lo: f64,
    pub hi: f64,
}

impl Axis {
    pub fn new(at: f64, lo: f64, hi: f64) -> Self {
        Self { at, lo, hi }
    }
}

// Corner arrays are nested outer→inner as `[t][z][x][y]`, where index 0 is the
// lower sample on that axis and index 1 the upper. Because each level is just a
// fixed-size array of the level below, every helper collapses its outermost
// axis and delegates to the next: `trilinear` slices `corners[zi]` straight
// into `bilinear`, and so on — no manual corner re-indexing.

/// Blend two corners along a single axis.
pub fn lerp(axis: Axis, lo: &Weather, hi: &Weather) -> Weather {
    let Axis { at, lo: a, hi: b } = axis;
    // `at` sitting on the lower sample, or a degenerate (lo == hi) axis, both
    // collapse to the lower corner — no interpolation possible or needed.
    if (at - a).abs() < f64::EPSILON || (a - b).abs() < f64::EPSILON {
        lo.clone()
    } else {
        // Blend in f64 (the components upcast exactly), store the result back as
        // f32 — compute precision and storage precision stay separate.
        let ratio = (at - a) / (b - a);
        let blend = |l: f32, h: f32| (f64::from(l) * (1.0 - ratio) + f64::from(h) * ratio) as f32;
        Weather { u_wind: blend(lo.u_wind, hi.u_wind), v_wind: blend(lo.v_wind, hi.v_wind) }
    }
}

/// Interpolate within an (x, y) cell. `c` is indexed `[xi][yi]`.
pub fn bilinear(x: Axis, y: Axis, c: [[&Weather; 2]; 2]) -> Weather {
    let at_x_lo = lerp(y, c[0][0], c[0][1]);
    let at_x_hi = lerp(y, c[1][0], c[1][1]);
    lerp(x, &at_x_lo, &at_x_hi)
}

/// Interpolate within an (x, y, z) cell. `c` is indexed `[zi][xi][yi]`.
pub fn trilinear(x: Axis, y: Axis, z: Axis, c: [[[&Weather; 2]; 2]; 2]) -> Weather {
    let at_z_lo = bilinear(x, y, c[0]);
    let at_z_hi = bilinear(x, y, c[1]);
    lerp(z, &at_z_lo, &at_z_hi)
}

/// Interpolate within an (x, y, z, t) cell. `c` is indexed `[ti][zi][xi][yi]`.
pub fn quadrilinear(
    x: Axis,
    y: Axis,
    z: Axis,
    t: Axis,
    c: [[[[&Weather; 2]; 2]; 2]; 2],
) -> Weather {
    let at_t_lo = trilinear(x, y, z, c[0]);
    let at_t_hi = trilinear(x, y, z, c[1]);
    lerp(t, &at_t_lo, &at_t_hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_blends_along_one_axis() {
        let w11 = Weather { u_wind: 5., v_wind: -4. };
        let w12 = Weather { u_wind: 10., v_wind: -2. };

        assert_eq!(lerp(Axis::new(1., 1., 2.), &w11, &w12), w11, "first point exactly match case");
        assert_eq!(lerp(Axis::new(2., 1., 2.), &w11, &w12), w12, "second point exactly match case");
        assert_eq!(
            lerp(Axis::new(1.5, 1., 2.), &w11, &w12),
            Weather { u_wind: 7.5, v_wind: -3. },
            "point is in the middle"
        );
        assert_eq!(
            lerp(Axis::new(1.9, 1., 2.), &w11, &w12),
            Weather { u_wind: 9.5, v_wind: -2.2 },
            "90% shift"
        );

        assert_eq!(
            lerp(Axis::new(1., 1., 2.), &w11, &w11.clone()),
            Weather { u_wind: 5., v_wind: -4. },
            "duplicates case1"
        );
        assert_eq!(
            lerp(Axis::new(2., 1., 2.), &w11, &w11.clone()),
            Weather { u_wind: 5., v_wind: -4. },
            "duplicates case2"
        );
        assert_eq!(
            lerp(Axis::new(1.5, 1., 2.), &w11, &w11.clone()),
            Weather { u_wind: 5., v_wind: -4. },
            "duplicates case3"
        );

        let w12 = Weather { u_wind: 0., v_wind: 0. };
        assert_eq!(
            lerp(Axis::new(1.5, 1., 2.), &w12, &w11),
            Weather { u_wind: 2.5, v_wind: -2.0 },
            "first weather contains zeros"
        );
        assert_eq!(
            lerp(Axis::new(1.5, 1., 2.), &w11, &w12),
            Weather { u_wind: 2.5, v_wind: -2.0 },
            "second weather contains zeros"
        );
        assert_eq!(
            lerp(Axis::new(1.5, 1., 2.), &w12, &w12.clone()),
            w12,
            "both weathers contains zeros"
        );
    }

    #[test]
    fn bilinear_blends_in_xy_plane() {
        let w11 = Weather { u_wind: 7.5, v_wind: -5.0 };
        let w12 = Weather { u_wind: 10.0, v_wind: -7.0 };
        let w21 = Weather { u_wind: -2.5, v_wind: -8.0 };
        let w22 = Weather { u_wind: -10.0, v_wind: -12.0 };
        // Corners indexed [xi][yi].
        let c = [[&w11, &w12], [&w21, &w22]];

        assert_eq!(
            bilinear(Axis::new(1., 1., 2.), Axis::new(1., 1., 2.), c),
            w11,
            "top-left point exactly match case"
        );
        assert_eq!(
            bilinear(Axis::new(2., 1., 2.), Axis::new(2., 1., 2.), c),
            w22,
            "bottom-right point exactly match"
        );
        assert_eq!(
            bilinear(Axis::new(1.5, 1., 2.), Axis::new(1., 1., 2.), c),
            lerp(Axis::new(1.5, 1., 2.), &w11, &w21),
            "point is on the middle of the left (self.lon == left_lon)"
        );
        assert_eq!(
            bilinear(Axis::new(1.5, 1., 2.), Axis::new(2., 1., 2.), c),
            lerp(Axis::new(1.5, 1., 2.), &w12, &w22),
            "point is on the middle of the right (self.lon == right_lon)"
        );
        assert_eq!(
            bilinear(Axis::new(2., 1., 2.), Axis::new(1.5, 1., 2.), c),
            lerp(Axis::new(1.5, 1., 2.), &w21, &w22),
            "point is on the middle of the top (self.lat == top_lat)"
        );
        assert_eq!(
            bilinear(Axis::new(1., 1., 2.), Axis::new(1.5, 1., 2.), c),
            lerp(Axis::new(1.5, 1., 2.), &w11, &w12),
            "point is on the middle of the bottom (self.lat == bottom_lat)"
        );
        assert_eq!(
            bilinear(Axis::new(1.5, 1., 2.), Axis::new(1.5, 1., 2.), c),
            Weather { u_wind: 1.25, v_wind: -8.0 },
            "point is in the center"
        );
    }

    #[test]
    fn trilinear_blends_across_pressure() {
        let w111 = Weather { u_wind: 10.0, v_wind: 5.0 };
        let w112 = Weather { u_wind: 12.0, v_wind: 6.0 };
        let w121 = Weather { u_wind: 14.0, v_wind: 7.0 };
        let w122 = Weather { u_wind: 16.0, v_wind: 8.0 };

        let w211 = Weather { u_wind: 11.0, v_wind: 5.5 };
        let w212 = Weather { u_wind: 13.0, v_wind: 6.5 };
        let w221 = Weather { u_wind: 15.0, v_wind: 7.5 };
        let w222 = Weather { u_wind: 17.0, v_wind: 8.5 };
        // Corners indexed [zi][xi][yi].
        let cube = [[[&w111, &w112], [&w121, &w122]], [[&w211, &w212], [&w221, &w222]]];

        assert_eq!(
            trilinear(Axis::new(1., 1., 2.), Axis::new(1., 1., 2.), Axis::new(1., 1., 5.), cube),
            w111,
            "point exactly match case"
        );
        assert_eq!(
            trilinear(Axis::new(1.5, 1., 2.), Axis::new(1., 1., 2.), Axis::new(1., 1., 5.), cube),
            lerp(Axis::new(1.5, 1., 2.), &w111, &w121),
            "point is on the middle of the left (self.lon == left_lon) and lowest pressure"
        );
        assert_eq!(
            trilinear(Axis::new(1.5, 1., 2.), Axis::new(1.5, 1., 2.), Axis::new(1., 1., 5.), cube),
            bilinear(
                Axis::new(1.5, 1., 2.),
                Axis::new(1.5, 1., 2.),
                [[&w111, &w112], [&w121, &w122]]
            ),
            "point is in the center and lowest pressure"
        );
        assert_eq!(
            trilinear(Axis::new(1.5, 1., 2.), Axis::new(1.5, 1., 2.), Axis::new(5., 0., 10.), cube),
            Weather { u_wind: 13.5, v_wind: 6.75 },
            "point is in the center and middle pressure"
        );

        // Fully degenerate cell (every axis lo == hi): must return the sample.
        let same = [[[&w111, &w111], [&w111, &w111]], [[&w111, &w111], [&w111, &w111]]];
        assert_eq!(
            trilinear(
                Axis::new(51.25, 51.25, 51.25),
                Axis::new(30.25, 30.25, 30.25),
                Axis::new(100300., 100000., 100000.),
                same,
            ),
            Weather { u_wind: 10., v_wind: 5. },
            "point with the same data"
        );
    }

    #[test]
    fn quadrilinear_blends_across_time() {
        let t1 = 10000.0;
        let t2 = 20000.0;

        let w1111 = Weather { u_wind: 10.0, v_wind: 5.0 };
        let w1112 = Weather { u_wind: 12.0, v_wind: 6.0 };
        let w1121 = Weather { u_wind: 14.0, v_wind: 7.0 };
        let w1122 = Weather { u_wind: 16.0, v_wind: 8.0 };

        let w1211 = Weather { u_wind: 11.0, v_wind: 5.5 };
        let w1212 = Weather { u_wind: 13.0, v_wind: 6.5 };
        let w1221 = Weather { u_wind: 15.0, v_wind: 7.5 };
        let w1222 = Weather { u_wind: 17.0, v_wind: 8.5 };

        let w2111 = Weather { u_wind: 10.5, v_wind: 5.25 };
        let w2112 = Weather { u_wind: 12.5, v_wind: 6.25 };
        let w2121 = Weather { u_wind: 14.5, v_wind: 7.25 };
        let w2122 = Weather { u_wind: 16.5, v_wind: 8.25 };

        let w2211 = Weather { u_wind: 11.5, v_wind: 5.75 };
        let w2212 = Weather { u_wind: 13.5, v_wind: 6.75 };
        let w2221 = Weather { u_wind: 15.5, v_wind: 7.75 };
        let w2222 = Weather { u_wind: 17.5, v_wind: 8.75 };

        // Corners indexed [ti][zi][xi][yi].
        let hyper = [
            [[[&w1111, &w1112], [&w1121, &w1122]], [[&w1211, &w1212], [&w1221, &w1222]]],
            [[[&w2111, &w2112], [&w2121, &w2122]], [[&w2211, &w2212], [&w2221, &w2222]]],
        ];

        assert_eq!(
            quadrilinear(
                Axis::new(1., 1., 2.),
                Axis::new(1., 1., 2.),
                Axis::new(1., 1., 9.),
                Axis::new(t1, t1, t2),
                hyper,
            ),
            w1111,
            "point exactly match, full of sames case"
        );

        let result = quadrilinear(
            Axis::new(1., 1., 2.),
            Axis::new(1., 1., 2.),
            Axis::new(1., 1., 9.),
            Axis::new(t1 + 5000.0, t1, t2),
            hyper,
        );
        assert!((result.u_wind - 10.25).abs() < 1e-6, "half time case: {result:?}");
        assert!((result.v_wind - 5.125).abs() < 1e-6, "half time case: {result:?}");
    }
}
