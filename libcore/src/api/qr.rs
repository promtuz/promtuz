//! QR rendering helper for the client.

/// Per-cell mask of outer corners to round for a row-major `size`×`size` grid, non-zero meaning
/// dark. Finder patterns are skipped; a mis-sized grid gets an all-zero mask.
#[uniffi::export]
pub fn compute_qr_mask(grid: Vec<u8>, size: u32) -> Vec<u8> {
    let n = size as usize;
    if n == 0 || grid.len() != n * n {
        return vec![0u8; grid.len()];
    }

    let mut out = vec![0u8; grid.len()];
    for y in 0..n {
        for x in 0..n {
            let i = y * n + x;
            if grid[i] == 0 || is_finder(x, y, n) {
                continue;
            }

            let n_ = y > 0     && grid[(y - 1) * n + x] != 0;
            let s  = y + 1 < n && grid[(y + 1) * n + x] != 0;
            let w_ = x > 0     && grid[y * n + (x - 1)] != 0;
            let e  = x + 1 < n && grid[y * n + (x + 1)] != 0;

            let mut mask = 0u8;
            if !n_ && !w_ { mask |= 0b0001; } // TL
            if !n_ && !e  { mask |= 0b0010; } // TR
            if !s  && !e  { mask |= 0b0100; } // BR
            if !s  && !w_ { mask |= 0b1000; } // BL
            out[i] = mask;
        }
    }
    out
}

#[inline(always)]
fn is_finder(x: usize, y: usize, n: usize) -> bool {
    let f = 7;
    !(y >= f || x >= f && x < n - f) || (x < f && y >= n - f)
}
