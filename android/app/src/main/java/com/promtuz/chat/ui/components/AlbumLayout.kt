package com.promtuz.chat.ui.components

import kotlin.math.abs
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/** One tile of an album. [x] and [w] are fractions of the album width; [y] and [h] of its max height. */
class AlbumCell(val x: Float, val y: Float, val w: Float, val h: Float)

private const val MAX_W = 800f
private const val MAX_H = 814f
private const val MIN_W = 120f * MAX_W / 360f  // 120dp at a 360dp-wide layout
private const val PADDINGS_W = 40f * MAX_W / 360f
private const val MIN_H = 100f / MAX_H
private const val MIN_HEIGHT = 120f * MAX_W / 360f

/** The album's height as a fraction of the width it was laid out for. */
const val ALBUM_HEIGHT_RATIO = MAX_H / MAX_W

/**
 * Album grid. Two, three and four
 * pictures get the hand-picked layouts keyed on how wide, narrow or square each one is;
 * anything else, or any panorama, is fitted by trying every row split of up to three per
 * row and keeping the one closest to a 4:3 stack.
 */
fun albumLayout(ratios: List<Float>): List<AlbumCell> {
    val count = ratios.size
    if (count == 0) return emptyList()
    if (count == 1) {
        val h = min(MAX_W / ratios[0], MAX_H / 2f)
        return listOf(AlbumCell(0f, 0f, 1f, h / MAX_H))
    }
    val proportions = ratios.joinToString("") { if (it > 1.2f) "w" else if (it < 0.8f) "n" else "q" }
    val average = (1f + ratios.sum()) / count
    val force = ratios.any { it > 2f }
    val maxAspect = MAX_W / MAX_H

    fun cell(x: Float, y: Float, w: Float, h: Float) = AlbumCell(x / MAX_W, y, w / MAX_W, h)

    if (!force && count == 2) {
        val (r1, r2) = ratios
        return when {
            proportions == "ww" && average > 1.4f * maxAspect && r1 - r2 < 0.2f -> {
                val h = min(MAX_W / r1, min(MAX_W / r2, MAX_H / 2f)).roundToInt() / MAX_H
                listOf(cell(0f, 0f, MAX_W, h), cell(0f, h, MAX_W, h))
            }
            proportions == "ww" || proportions == "qq" -> {
                val w = MAX_W / 2f
                val h = min(w / r1, min(w / r2, MAX_H)).roundToInt() / MAX_H
                listOf(cell(0f, 0f, w, h), cell(w, 0f, w, h))
            }
            else -> {
                var second = max(0.4f * MAX_W, (MAX_W / r1 / (1f / r1 + 1f / r2)).roundToInt().toFloat())
                var first = MAX_W - second
                if (first < MIN_W) { second -= MIN_W - first; first = MIN_W }
                val h = min(MAX_H, min(first / r1, second / r2).roundToInt().toFloat()) / MAX_H
                listOf(cell(0f, 0f, first, h), cell(first, 0f, second, h))
            }
        }
    }
    if (!force && count == 3) {
        val (r1, r2, r3) = ratios
        return if (proportions[0] == 'n') {
            val third = min(MAX_H * 0.5f, (r2 * MAX_W / (r3 + r2)).roundToInt().toFloat())
            val second = MAX_H - third
            val right = max(MIN_W, min(MAX_W * 0.5f, min(third * r3, second * r2).roundToInt().toFloat()))
            val left = min(MAX_H * r1 + PADDINGS_W, MAX_W - right).roundToInt().toFloat()
            listOf(
                cell(0f, 0f, left, 1f),
                cell(MAX_W - right, 0f, right, second / MAX_H),
                cell(MAX_W - right, second / MAX_H, right, third / MAX_H),
            )
        } else {
            val first = min(MAX_W / r1, MAX_H * 0.66f).roundToInt() / MAX_H
            val w = MAX_W / 2f
            val second = max(MIN_H, min(MAX_H - first * MAX_H, min(w / r2, w / r3).roundToInt().toFloat()) / MAX_H)
            listOf(cell(0f, 0f, MAX_W, first), cell(0f, first, w, second), cell(w, first, w, second))
        }
    }
    if (!force && count == 4) {
        val (r1, r2, r3, r4) = ratios
        return if (proportions[0] == 'w') {
            val h0 = min(MAX_W / r1, MAX_H * 0.66f).roundToInt() / MAX_H
            var h = (MAX_W / (r2 + r3 + r4)).roundToInt().toFloat()
            var w0 = max(MIN_W, min(MAX_W * 0.4f, h * r2))
            var w2 = max(max(MIN_W, MAX_W * 0.33f), h * r4)
            var w1 = MAX_W - w0 - w2
            val minMiddle = 58f * MAX_W / 360f
            if (w1 < minMiddle) { val d = minMiddle - w1; w1 = minMiddle; w0 -= d / 2; w2 -= d - d / 2 }
            h = min(MAX_H - h0 * MAX_H, h)
            val hf = max(MIN_H, h / MAX_H)
            listOf(cell(0f, 0f, MAX_W, h0), cell(0f, h0, w0, hf), cell(w0, h0, w1, hf), cell(w0 + w1, h0, w2, hf))
        } else {
            val w = max(MIN_W, (MAX_H / (1f / r2 + 1f / r3 + 1f / r4)).roundToInt().toFloat())
            val h0 = min(0.33f, max(MIN_HEIGHT, w / r2) / MAX_H)
            val h1 = min(0.33f, max(MIN_HEIGHT, w / r3) / MAX_H)
            val h2 = 1f - h0 - h1
            val w0 = min(MAX_H * r1 + PADDINGS_W, MAX_W - w).roundToInt().toFloat()
            listOf(
                cell(0f, 0f, w0, h0 + h1 + h2),
                cell(MAX_W - w, 0f, w, h0), cell(MAX_W - w, h0, w, h1), cell(MAX_W - w, h0 + h1, w, h2),
            )
        }
    }

    // General case: cropped ratios, every row split, closest stack to 4:3 wins.
    val cropped = ratios.map { r ->
        val c = if (average > 1.1f) max(1f, r) else min(1f, r)
        max(0.66667f, min(1.7f, c))
    }
    fun rowHeight(from: Int, to: Int) = MAX_W / cropped.subList(from, to).sum()
    val attempts = ArrayList<Pair<IntArray, FloatArray>>()
    val n = cropped.size
    for (a in 1 until n) {
        val b = n - a
        if (a > 3 || b > 3) continue
        attempts += intArrayOf(a, b) to floatArrayOf(rowHeight(0, a), rowHeight(a, n))
    }
    for (a in 1 until n - 1) for (b in 1 until n - a) {
        val c = n - a - b
        if (a > 3 || b > (if (average < 0.85f) 4 else 3) || c > 3) continue
        attempts += intArrayOf(a, b, c) to floatArrayOf(rowHeight(0, a), rowHeight(a, a + b), rowHeight(a + b, n))
    }
    for (a in 1 until n - 2) for (b in 1 until n - a) for (c in 1 until n - a - b) {
        val d = n - a - b - c
        if (a > 3 || b > 3 || c > 3 || d > 3) continue
        attempts += intArrayOf(a, b, c, d) to
            floatArrayOf(rowHeight(0, a), rowHeight(a, a + b), rowHeight(a + b, a + b + c), rowHeight(a + b + c, n))
    }
    val maxHeight = MAX_W / 3f * 4f
    var best: Pair<IntArray, FloatArray>? = null
    var bestDiff = 0f
    for (attempt in attempts) {
        val (counts, heights) = attempt
        var diff = abs(heights.sum() - maxHeight)
        if (counts.size > 1 && (counts[0] > counts[1] || (counts.size > 2 && counts[1] > counts[2]) ||
                (counts.size > 3 && counts[2] > counts[3]))) diff *= 1.2f
        if (heights.min() < MIN_W) diff *= 1.5f
        if (best == null || diff < bestDiff) { best = attempt; bestDiff = diff }
    }
    // Past twelve no split of rows fits; the cap keeps new albums under it, older ones get plain rows.
    val (counts, heights) = best ?: run {
        val rows = (n + 2) / 3
        val rowCounts = IntArray(rows) { r -> minOf(3, n - r * 3) }
        var from = 0
        rowCounts to FloatArray(rows) { r -> rowHeight(from, from + rowCounts[r]).also { from += rowCounts[r] } }
    }
    val cells = ArrayList<AlbumCell>(n)
    var index = 0
    var y = 0f
    for (i in counts.indices) {
        val lineHeight = heights[i]
        var x = 0f
        val widths = FloatArray(counts[i]) { k -> (cropped[index + k] * lineHeight).toInt().toFloat() }
        widths[widths.lastIndex] += MAX_W - widths.sum()
        for (k in 0 until counts[i]) {
            cells += cell(x, y / MAX_H, widths[k], max(MIN_H, lineHeight / MAX_H))
            x += widths[k]
            index++
        }
        y += lineHeight
    }
    return cells
}
