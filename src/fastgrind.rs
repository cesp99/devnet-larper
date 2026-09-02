// Fast "AAA" pubkey grinder: extended-coordinate point stepping with batched inversion.
use crate::consts::*;
use crate::{aaa_range, GroundKey};
use curve25519_dalek::constants::ED25519_BASEPOINT_TABLE;
use curve25519_dalek::scalar::Scalar;
use fiat_crypto::curve25519_64::*;
use solana_sdk::pubkey::Pubkey;
use std::sync::mpsc::SyncSender;

#[derive(Clone, Copy)]
pub struct Fe(fiat_25519_tight_field_element);

impl Fe {
    fn zero() -> Fe {
        Fe(fiat_25519_tight_field_element([0; 5]))
    }
    pub fn from_bytes(b: &[u8; 32]) -> Fe {
        let mut o = fiat_25519_tight_field_element([0; 5]);
        fiat_25519_from_bytes(&mut o, b);
        Fe(o)
    }
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut o = [0u8; 32];
        fiat_25519_to_bytes(&mut o, &self.0);
        o
    }
    fn loose(&self) -> fiat_25519_loose_field_element {
        let mut o = fiat_25519_loose_field_element([0; 5]);
        fiat_25519_relax(&mut o, &self.0);
        o
    }
    pub fn mul(&self, b: &Fe) -> Fe {
        let mut o = fiat_25519_tight_field_element([0; 5]);
        fiat_25519_carry_mul(&mut o, &self.loose(), &b.loose());
        Fe(o)
    }
    pub fn sq(&self) -> Fe {
        let mut o = fiat_25519_tight_field_element([0; 5]);
        fiat_25519_carry_square(&mut o, &self.loose());
        Fe(o)
    }
    pub fn add(&self, b: &Fe) -> Fe {
        let mut l = fiat_25519_loose_field_element([0; 5]);
        fiat_25519_add(&mut l, &self.0, &b.0);
        let mut o = fiat_25519_tight_field_element([0; 5]);
        fiat_25519_carry(&mut o, &l);
        Fe(o)
    }
    pub fn sub(&self, b: &Fe) -> Fe {
        let mut l = fiat_25519_loose_field_element([0; 5]);
        fiat_25519_sub(&mut l, &self.0, &b.0);
        let mut o = fiat_25519_tight_field_element([0; 5]);
        fiat_25519_carry(&mut o, &l);
        Fe(o)
    }
    pub fn neg(&self) -> Fe {
        Fe::zero().sub(self)
    }
    pub fn eq(&self, b: &Fe) -> bool {
        self.to_bytes() == b.to_bytes()
    }
    /// self^e for a little-endian exponent.
    fn pow_le(&self, e: &[u8; 32]) -> Fe {
        let mut result = Fe::from_bytes(&{
            let mut one = [0u8; 32];
            one[0] = 1;
            one
        });
        for i in (0..255).rev() {
            result = result.sq();
            if (e[i / 8] >> (i % 8)) & 1 == 1 {
                result = result.mul(self);
            }
        }
        result
    }
    pub fn invert(&self) -> Fe {
        // p - 2 = 2^255 - 21
        let mut e = [0xffu8; 32];
        e[0] = 0xeb;
        e[31] = 0x7f;
        self.pow_le(&e)
    }
    fn pow_p58(&self) -> Fe {
        // (p - 5) / 8 = 2^252 - 3
        let mut e = [0xffu8; 32];
        e[0] = 0xfd;
        e[31] = 0x0f;
        self.pow_le(&e)
    }
}

#[derive(Clone, Copy)]
pub struct P3 {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

pub struct Niels {
    ypx: Fe,
    ymx: Fe,
    xy2d: Fe,
}

pub fn basepoint_niels() -> Niels {
    Niels {
        ypx: Fe::from_bytes(&B_YPX),
        ymx: Fe::from_bytes(&B_YMX),
        xy2d: Fe::from_bytes(&B_XY2D),
    }
}

/// p + q where q is an affine (Niels-form) point.
pub fn madd(p: &P3, q: &Niels) -> P3 {
    let ypx = p.y.add(&p.x);
    let ymx = p.y.sub(&p.x);
    let a = ypx.mul(&q.ypx);
    let b = ymx.mul(&q.ymx);
    let c = q.xy2d.mul(&p.t);
    let d = p.z.add(&p.z);
    let x3 = a.sub(&b);
    let y3 = a.add(&b);
    let z3 = d.add(&c);
    let t3 = d.sub(&c);
    P3 {
        x: x3.mul(&t3),
        y: y3.mul(&z3),
        z: z3.mul(&t3),
        t: x3.mul(&y3),
    }
}

/// Decompress a canonical ed25519 point encoding.
pub fn decompress(bytes: &[u8; 32]) -> Option<P3> {
    let mut yb = *bytes;
    let sign = yb[31] >> 7;
    yb[31] &= 0x7f;
    let one = Fe::from_bytes(&{
        let mut o = [0u8; 32];
        o[0] = 1;
        o
    });
    let d = Fe::from_bytes(&D);
    let y = Fe::from_bytes(&yb);
    let yy = y.sq();
    let u = yy.sub(&one);
    let v = d.mul(&yy).add(&one);
    let v3 = v.sq().mul(&v);
    let v7 = v3.sq().mul(&v);
    let mut x = u.mul(&v3).mul(&u.mul(&v7).pow_p58());
    let vxx = v.mul(&x.sq());
    if !vxx.eq(&u) {
        if vxx.eq(&u.neg()) {
            x = x.mul(&Fe::from_bytes(&SQRT_M1));
        } else {
            return None;
        }
    }
    if (x.to_bytes()[0] & 1) != sign {
        x = x.neg();
    }
    Some(P3 {
        x,
        y,
        z: one,
        t: x.mul(&y),
    })
}

/// Compress a batch of projective points using one field inversion.
pub fn batch_compress(points: &[P3], out: &mut Vec<[u8; 32]>) {
    let mut zinvs = vec![];
    batch_y(points, out, &mut zinvs);
    for i in 0..points.len() {
        let x = points[i].x.mul(&zinvs[i]);
        out[i][31] |= (x.to_bytes()[0] & 1) << 7;
    }
}

/// y-coordinate bytes (sign bit clear) for a batch, plus each point's 1/Z.
pub fn batch_y(points: &[P3], out: &mut Vec<[u8; 32]>, zinvs: &mut Vec<Fe>) {
    let n = points.len();
    let mut prefix: Vec<Fe> = Vec::with_capacity(n);
    let mut acc = Fe::from_bytes(&{
        let mut o = [0u8; 32];
        o[0] = 1;
        o
    });
    for p in points {
        prefix.push(acc);
        acc = acc.mul(&p.z);
    }
    let mut inv = acc.invert();
    out.clear();
    out.resize(n, [0u8; 32]);
    zinvs.clear();
    zinvs.resize(n, Fe::zero());
    for i in (0..n).rev() {
        let zinv = inv.mul(&prefix[i]);
        inv = inv.mul(&points[i].z);
        zinvs[i] = zinv;
        out[i] = points[i].y.mul(&zinv).to_bytes();
    }
}

fn dalek_compressed(s: &Scalar) -> [u8; 32] {
    (s * &ED25519_BASEPOINT_TABLE).compress().to_bytes()
}

/// Sanity check of the custom arithmetic against curve25519-dalek.
pub fn self_test() {
    use rand::{RngCore, SeedableRng};
    let mut rng = rand::rngs::SmallRng::from_entropy();
    let base = basepoint_niels();
    for _ in 0..4 {
        let mut buf = [0u8; 32];
        rng.fill_bytes(&mut buf);
        let s = Scalar::from_bytes_mod_order(buf);
        let mut p = decompress(&dalek_compressed(&s)).expect("decompress");
        let mut pts = vec![];
        for _ in 0..40 {
            p = madd(&p, &base);
            pts.push(p);
        }
        let mut out = vec![];
        batch_compress(&pts, &mut out);
        for (i, b) in out.iter().enumerate() {
            let expect = dalek_compressed(&(s + Scalar::from((i + 1) as u64)));
            assert_eq!(*b, expect, "fast grinder arithmetic mismatch");
        }
    }
    println!("fast grinder self-test passed");
}

pub fn grind_thread(tx: SyncSender<GroundKey>) {
    use rand::{RngCore, SeedableRng};
    let mut rng = rand::rngs::SmallRng::from_entropy();
    let (mut lo, mut hi) = aaa_range();
    lo[31] &= 0x7f;
    hi[31] &= 0x7f;
    let base = basepoint_niels();
    const N: usize = 512;
    let mut zinvs: Vec<Fe> = Vec::with_capacity(N);
    let mut buf = [0u8; 32];
    rng.fill_bytes(&mut buf);
    let mut scalar = Scalar::from_bytes_mod_order(buf);
    let mut p = decompress(&dalek_compressed(&scalar)).expect("decompress start");
    let mut pts: Vec<P3> = Vec::with_capacity(N);
    let mut out: Vec<[u8; 32]> = Vec::with_capacity(N);
    loop {
        pts.clear();
        for _ in 0..N {
            pts.push(p);
            p = madd(&p, &base);
        }
        batch_y(&pts, &mut out, &mut zinvs);
        for i in 0..N {
            if out[i] >= lo && out[i] < hi {
                let x = pts[i].x.mul(&zinvs[i]);
                let mut full = out[i];
                full[31] |= (x.to_bytes()[0] & 1) << 7;
                let bytes = &full;
                let (lo_full, hi_full) = aaa_range();
                if !(*bytes >= lo_full && *bytes < hi_full) {
                    continue;
                }
                let s = scalar + Scalar::from(i as u64);
                let check = dalek_compressed(&s);
                if check != *bytes {
                    eprintln!("FATAL: grinder produced a key that does not match dalek; aborting");
                    std::process::exit(2);
                }
                let mut nonce = [0u8; 32];
                rng.fill_bytes(&mut nonce);
                let mut expanded_bytes = [0u8; 64];
                expanded_bytes[..32].copy_from_slice(&s.to_bytes());
                expanded_bytes[32..].copy_from_slice(&nonce);
                let expanded =
                    ed25519_dalek::ExpandedSecretKey::from_bytes(&expanded_bytes).unwrap();
                let dalek_pk = ed25519_dalek::PublicKey::from_bytes(bytes).unwrap();
                let key = GroundKey {
                    pk: Pubkey::new_from_array(*bytes),
                    expanded,
                    dalek_pk,
                };
                if tx.send(key).is_err() {
                    return;
                }
            }
        }
        scalar += Scalar::from(N as u64);
    }
}
