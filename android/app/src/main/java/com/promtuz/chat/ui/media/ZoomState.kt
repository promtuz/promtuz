package com.promtuz.chat.ui.media

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.AnimationSpec
import androidx.compose.animation.core.VectorConverter
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.geometry.center
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch

const val MAX_ZOOM = 5f
const val DOUBLE_TAP_ZOOM = 2.5f

/**
 * Scale and pan of one page. [offset] is where the picture's centre sits relative to the
 * viewport's centre, so a zoom about a finger keeps the pixel under it still.
 */
class ZoomState {
    var scale by mutableStateOf(1f)
        private set
    var offset by mutableStateOf(Offset.Zero)
        private set
    var viewport by mutableStateOf(Size.Zero)
    var fitted by mutableStateOf(Size.Zero)

    val zoomed: Boolean get() = scale > 1.001f

    private val scaleAnim = Animatable(1f)
    private val offsetAnim = Animatable(Offset.Zero, Offset.VectorConverter)

    /** The picture's on-screen rectangle in viewport coordinates. */
    fun displayed(): Rect {
        val size = fitted * scale
        val center = viewport.center + offset
        return Rect(center - Offset(size.width / 2f, size.height / 2f), size)
    }

    fun zoomBy(factor: Float, centroid: Offset) {
        val next = (scale * factor).coerceIn(MIN_OVERSHOOT, MAX_ZOOM)
        val about = centroid - viewport.center
        offset = about + (offset - about) * (next / scale)
        scale = next
        clamp(soft = true)
    }

    fun panBy(delta: Offset) {
        offset += delta
        clamp(soft = false)
    }

    /** How far a pan at the current edge would overshoot, so the caller can hand it to the pager. */
    fun overshootX(dx: Float): Float {
        val max = ((fitted.width * scale - viewport.width) / 2f).coerceAtLeast(0f)
        val next = offset.x + dx
        return when {
            next > max -> next - max
            next < -max -> next + max
            else -> 0f
        }
    }

    suspend fun animateTo(targetScale: Float, targetOffset: Offset, spec: AnimationSpec<Float>, offsetSpec: AnimationSpec<Offset>) =
        coroutineScope {
            scaleAnim.snapTo(scale)
            offsetAnim.snapTo(offset)
            launch { scaleAnim.animateTo(targetScale, spec) { scale = value } }
            launch { offsetAnim.animateTo(targetOffset, offsetSpec) { offset = value } }
        }

    /** Where a double tap should land: zoomed about the tap, or back to rest. */
    fun doubleTapTarget(tap: Offset): Pair<Float, Offset> {
        if (zoomed) return 1f to Offset.Zero
        val about = tap - viewport.center
        val target = about + (offset - about) * (DOUBLE_TAP_ZOOM / scale)
        return DOUBLE_TAP_ZOOM to clamped(target, DOUBLE_TAP_ZOOM)
    }

    /** Rest state after a pinch let go outside the allowed range. */
    fun settleTarget(): Pair<Float, Offset> {
        val s = scale.coerceIn(1f, MAX_ZOOM)
        return s to clamped(offset, s)
    }

    private fun clamp(soft: Boolean) {
        if (!soft) offset = clamped(offset, scale)
    }

    private fun clamped(o: Offset, s: Float): Offset {
        val maxX = ((fitted.width * s - viewport.width) / 2f).coerceAtLeast(0f)
        val maxY = ((fitted.height * s - viewport.height) / 2f).coerceAtLeast(0f)
        return Offset(o.x.coerceIn(-maxX, maxX), o.y.coerceIn(-maxY, maxY))
    }

    private companion object {
        const val MIN_OVERSHOOT = 0.6f
    }
}

private operator fun Size.times(k: Float) = Size(width * k, height * k)

/** The largest rectangle of [w]x[h] proportions centred in [viewport]. */
fun fitInto(w: Int, h: Int, viewport: Size): Rect {
    if (viewport.width <= 0f || viewport.height <= 0f) return Rect.Zero
    val ratio = if (w > 0 && h > 0) w.toFloat() / h else 1f
    val size = if (viewport.width / viewport.height > ratio) Size(viewport.height * ratio, viewport.height)
    else Size(viewport.width, viewport.width / ratio)
    return Rect(viewport.center - Offset(size.width / 2f, size.height / 2f), size)
}
