package com.promtuz.chat.ui.views

import android.content.Context
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import android.view.View
import androidx.annotation.OptIn
import androidx.camera.core.ExperimentalGetImage

@OptIn(ExperimentalGetImage::class)
class QrScannerOverlayView(
    context: Context
) : View(context) {
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
    }

    private val guideTopLeft = Path()
    private val guideTopRight = Path()
    private val guideBottomLeft = Path()
    private val guideBottomRight = Path()

    private val guideColor = Color.WHITE
    private val guideWidth = 6f
    private val guideSize = 600f
    private val guideCornerLength = 150f
    private val guideCornerRadius = 100f

    private val frameRunnable: Runnable = Runnable {
        invalidate()
        postOnAnimation(frameRunnable)
    }

    init {
        setWillNotDraw(false)
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        post(frameRunnable)
    }

    override fun onDetachedFromWindow() {
        removeCallbacks(frameRunnable)
        super.onDetachedFromWindow()
    }


    override fun onDraw(canvas: android.graphics.Canvas) {
        super.onDraw(canvas)

        paint.strokeWidth = guideWidth
        paint.color = guideColor

        val width = width.toFloat()
        val height = height.toFloat()

        // Calculate center position
        val left = (width - guideSize) / 2f
        val top = (height - guideSize) / 2f
        val right = left + guideSize
        val bottom = top + guideSize


        // Top-left corner bracket
        guideTopLeft.reset()
        guideTopLeft.moveTo(left + guideCornerLength, top)
        guideTopLeft.lineTo(left + guideCornerRadius, top)
        guideTopLeft.arcTo(
            left, top, left + guideCornerRadius * 2, top + guideCornerRadius * 2,
            270f, -90f, false
        )
        guideTopLeft.lineTo(left, top + guideCornerLength)
        canvas.drawPath(guideTopLeft, paint)


        // Top-right corner bracket
        guideTopRight.reset()
        guideTopRight.moveTo(right - guideCornerLength, top)
        guideTopRight.lineTo(right - guideCornerRadius, top)
        guideTopRight.arcTo(
            right - guideCornerRadius * 2, top, right, top + guideCornerRadius * 2,
            270f, 90f, false
        )
        guideTopRight.lineTo(right, top + guideCornerLength)
        canvas.drawPath(guideTopRight, paint)


        // Bottom-left corner bracket
        guideBottomLeft.reset()
        guideBottomLeft.moveTo(left, bottom - guideCornerLength)
        guideBottomLeft.lineTo(left, bottom - guideCornerRadius)
        guideBottomLeft.arcTo(
            left, bottom - guideCornerRadius * 2, left + guideCornerRadius * 2, bottom,
            180f, -90f, false
        )
        guideBottomLeft.lineTo(left + guideCornerLength, bottom)
        canvas.drawPath(guideBottomLeft, paint)


        // Bottom-right corner bracket
        guideBottomRight.reset()
        guideBottomRight.moveTo(right, bottom - guideCornerLength)
        guideBottomRight.lineTo(right, bottom - guideCornerRadius)
        guideBottomRight.arcTo(
            right - guideCornerRadius * 2, bottom - guideCornerRadius * 2, right, bottom,
            0f, 90f, false
        )
        guideBottomRight.lineTo(right - guideCornerLength, bottom)
        canvas.drawPath(guideBottomRight, paint)
    }
}