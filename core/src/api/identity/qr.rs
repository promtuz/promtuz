use jni::JNIEnv;
use jni::objects::JByteArray;
use jni_macro::jni;

use crate::JC;

#[inline(always)]
fn is_finder(x: usize, y: usize, n: usize) -> bool {
    let f = 7;
    !(y >= f || x >= f && x < n - f) || (x < f && y >= n - f)
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn computeQrMask<'a>(
    env: JNIEnv<'a>, _: JC<'a>, grid: JByteArray<'a>, size: i32,
) -> JByteArray<'a> {
    let n = size as usize;

    // Read input grid
    let buf = env.convert_byte_array(grid).expect("invalid grid byte[]");

    debug_assert!(buf.len() == n * n);

    let mut out = vec![0u8; buf.len()];

    for y in 0..n {
        for x in 0..n {
            let i = y * n + x;

            if buf[i] == 0 || is_finder(x, y, n) {
                continue;
            }

            let n_ = y > 0     && buf[(y - 1) * n + x] != 0;
            let s  = y + 1 < n && buf[(y + 1) * n + x] != 0;
            let w_ = x > 0     && buf[y * n + (x - 1)] != 0;
            let e  = x + 1 < n && buf[y * n + (x + 1)] != 0;

            let mut mask = 0u8;

            if !n_ && !w_ { mask |= 0b0001; } // TL
            if !n_ && !e  { mask |= 0b0010; } // TR
            if !s  && !e  { mask |= 0b0100; } // BR
            if !s  && !w_ { mask |= 0b1000; } // BL

            out[i] = mask;
        }
    }

    env.byte_array_from_slice(&out)
        .expect("failed to allocate result")
}
