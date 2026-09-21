package com.promtuz.chat.utils.media

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Matrix
import android.graphics.RectF

/** Source-relative centre and zoom, independent of the editor's size and screen orientation. */
data class PhotoCrop(val centerX: Float = .5f, val centerY: Float = .5f, val zoom: Float = 1f) {
    fun side(width: Int, height: Int): Float = minOf(width, height) / zoom

    fun bounded(width: Int, height: Int): PhotoCrop {
        val z = zoom.coerceIn(1f, 5f)
        val half = minOf(width, height) / z / 2f
        return PhotoCrop(
            centerX.coerceIn(half / width, 1f - half / width),
            centerY.coerceIn(half / height, 1f - half / height), z,
        )
    }

    fun render(source: Bitmap): Bitmap {
        val crop = bounded(source.width, source.height)
        val half = crop.side(source.width, source.height) / 2f
        val x = crop.centerX * source.width
        val y = crop.centerY * source.height
        return Bitmap.createBitmap(OUTPUT_EDGE, OUTPUT_EDGE, Bitmap.Config.ARGB_8888).also {
            val matrix = Matrix().apply {
                setRectToRect(RectF(x - half, y - half, x + half, y + half),
                    RectF(0f, 0f, OUTPUT_EDGE.toFloat(), OUTPUT_EDGE.toFloat()), Matrix.ScaleToFit.FILL)
            }
            Canvas(it).drawBitmap(source, matrix, Paint(Paint.ANTI_ALIAS_FLAG or Paint.FILTER_BITMAP_FLAG))
        }
    }

    companion object { const val OUTPUT_EDGE = 256 }
}
