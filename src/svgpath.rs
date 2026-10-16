//! Mini-parseur de l'attribut `d` d'un `<path>` SVG vers une géométrie
//! Direct2D. Couvre tout le langage de tracé (M L H V C S Q T A Z, absolus et
//! relatifs), assez pour dessiner des logos vectoriels nets à toute taille.

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::core::Result;
use windows_numerics::Vector2;

struct Tokens<'a> {
    s: &'a [u8],
    i: usize,
}

impl Tokens<'_> {
    fn skip_separators(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b',' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
        }
    }

    /// Prochaine commande, si le prochain élément en est une.
    fn command(&mut self) -> Option<u8> {
        self.skip_separators();
        let c = *self.s.get(self.i)?;
        c.is_ascii_alphabetic().then(|| {
            self.i += 1;
            c
        })
    }

    fn has_number(&mut self) -> bool {
        self.skip_separators();
        self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'))
    }

    fn number(&mut self) -> f32 {
        self.skip_separators();
        let start = self.i;
        let mut seen_dot = false;
        let mut seen_exp = false;
        if matches!(self.s.get(self.i), Some(b'-' | b'+')) {
            self.i += 1;
        }
        while let Some(&c) = self.s.get(self.i) {
            match c {
                b'0'..=b'9' => {}
                b'.' if !seen_dot && !seen_exp => seen_dot = true,
                b'e' | b'E' if !seen_exp => {
                    seen_exp = true;
                    if matches!(self.s.get(self.i + 1), Some(b'-' | b'+')) {
                        self.i += 1;
                    }
                }
                _ => break,
            }
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok().and_then(|n| n.parse().ok()).unwrap_or(0.0)
    }

    /// Drapeau d'arc : un seul caractère, souvent collé au nombre suivant (`013.04`).
    fn flag(&mut self) -> bool {
        self.skip_separators();
        let c = self.s.get(self.i).copied();
        self.i += 1;
        c == Some(b'1')
    }
}

fn v(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

/// Construit la géométrie du tracé, dans le repère du `viewBox` d'origine.
pub fn geometry(factory: &ID2D1Factory, d: &str, even_odd: bool) -> Result<ID2D1PathGeometry> {
    unsafe {
        let path = factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        sink.SetFillMode(if even_odd { D2D1_FILL_MODE_ALTERNATE } else { D2D1_FILL_MODE_WINDING });

        let mut t = Tokens { s: d.as_bytes(), i: 0 };
        let (mut cur, mut start) = ((0.0f32, 0.0f32), (0.0f32, 0.0f32));
        // Dernier point de contrôle, pour les courbes « lisses » S et T.
        let mut last_cubic: Option<(f32, f32)> = None;
        let mut last_quad: Option<(f32, f32)> = None;
        let mut open = false;
        let mut cmd = b'M';

        loop {
            if let Some(c) = t.command() {
                cmd = c;
            } else if !t.has_number() || cmd.eq_ignore_ascii_case(&b'Z') {
                // fin du tracé (ou nombres orphelins après Z : on ignore)
                break;
            }
            let rel = cmd.is_ascii_lowercase();
            let (ox, oy) = if rel { cur } else { (0.0, 0.0) };
            let point = |t: &mut Tokens| {
                let x = t.number() + ox;
                (x, t.number() + oy)
            };
            let (mut next_cubic, mut next_quad) = (None, None);
            let upper = cmd.to_ascii_uppercase();
            // Un tracé qui reprend après Z sans M repart du point de départ.
            if !open && upper != b'M' && upper != b'Z' {
                sink.BeginFigure(v(cur.0, cur.1), D2D1_FIGURE_BEGIN_FILLED);
                open = true;
            }
            match upper {
                b'M' => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                    }
                    cur = point(&mut t);
                    start = cur;
                    sink.BeginFigure(v(cur.0, cur.1), D2D1_FIGURE_BEGIN_FILLED);
                    open = true;
                    // Les paires suivantes sont des lignes implicites.
                    cmd = if rel { b'l' } else { b'L' };
                }
                b'L' => {
                    cur = point(&mut t);
                    sink.AddLine(v(cur.0, cur.1));
                }
                b'H' => {
                    cur.0 = t.number() + ox;
                    sink.AddLine(v(cur.0, cur.1));
                }
                b'V' => {
                    cur.1 = t.number() + oy;
                    sink.AddLine(v(cur.0, cur.1));
                }
                b'C' | b'S' => {
                    let c1 = if cmd.eq_ignore_ascii_case(&b'C') {
                        point(&mut t)
                    } else {
                        last_cubic.map_or(cur, |(x, y)| (2.0 * cur.0 - x, 2.0 * cur.1 - y))
                    };
                    let c2 = point(&mut t);
                    let end = point(&mut t);
                    sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                        point1: v(c1.0, c1.1),
                        point2: v(c2.0, c2.1),
                        point3: v(end.0, end.1),
                    });
                    next_cubic = Some(c2);
                    cur = end;
                }
                b'Q' | b'T' => {
                    let c = if cmd.eq_ignore_ascii_case(&b'Q') {
                        point(&mut t)
                    } else {
                        last_quad.map_or(cur, |(x, y)| (2.0 * cur.0 - x, 2.0 * cur.1 - y))
                    };
                    let end = point(&mut t);
                    sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                        point1: v(c.0, c.1),
                        point2: v(end.0, end.1),
                    });
                    next_quad = Some(c);
                    cur = end;
                }
                b'A' => {
                    let (rx, ry, rotation) = (t.number().abs(), t.number().abs(), t.number());
                    let (large, sweep) = (t.flag(), t.flag());
                    let end = point(&mut t);
                    sink.AddArc(&D2D1_ARC_SEGMENT {
                        point: v(end.0, end.1),
                        size: D2D_SIZE_F { width: rx, height: ry },
                        rotationAngle: rotation,
                        sweepDirection: if sweep {
                            D2D1_SWEEP_DIRECTION_CLOCKWISE
                        } else {
                            D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
                        },
                        arcSize: if large { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
                    });
                    cur = end;
                }
                b'Z' => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                        open = false;
                    }
                    cur = start;
                }
                _ => break, // commande inconnue : on s'arrête proprement
            }
            last_cubic = next_cubic;
            last_quad = next_quad;
        }
        if open {
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        }
        sink.Close()?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_compact_numbers_and_flags() {
        let mut t = Tokens { s: b"a6.105 6.105 0 013.046-.415.5e-1", i: 0 };
        assert_eq!(t.command(), Some(b'a'));
        assert_eq!((t.number(), t.number(), t.number()), (6.105, 6.105, 0.0));
        assert_eq!((t.flag(), t.flag()), (false, true));
        assert_eq!((t.number(), t.number(), t.number()), (3.046, -0.415, 0.05));
        assert!(!t.has_number());
    }
}
