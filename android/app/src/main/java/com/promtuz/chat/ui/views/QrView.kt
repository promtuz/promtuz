package com.promtuz.chat.ui.views

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Path
import android.graphics.PorterDuff
import android.graphics.PorterDuffXfermode
import android.graphics.RectF
import android.graphics.Shader
import android.view.View
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.common.BitMatrix
import com.google.zxing.qrcode.QRCodeWriter
import com.promtuz.core.API

class QrView(context: Context) : View(context) {

    var content = ByteArray(0)
    var color = Color.BLACK
    var loading = true

    private val writer = QRCodeWriter()
    private val hints = mapOf(
        EncodeHintType.CHARACTER_SET to "ISO-8859-1", EncodeHintType.MARGIN to 0
    )

    private var matrix: BitMatrix? = null

    private var roundMask: ByteArray? = null
    private var matrixSize = 0

    private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
    }
    private val xfermode = PorterDuffXfermode(PorterDuff.Mode.CLEAR)

    private fun isFinder(x: Int, y: Int, s: Int): Boolean {
        val f = 7
        return (x < f && y < f) || (x >= s - f && y < f) || (x < f && y >= s - f)
    }

    fun clear() {
        matrix = null
        roundMask = null
        loading = true
        content = ByteArray(0)
        invalidate()
    }

    fun regenerate() {
        if (content.isEmpty() || width <= 0) return

        matrix = writer.encode(
            content.toString(Charsets.ISO_8859_1), BarcodeFormat.QR_CODE, 0, 0, hints
        )
        matrixSize = matrix!!.width
        val n = matrixSize

        val grid = ByteArray(n * n)
        for (y in 0 until n) {
            for (x in 0 until n) {
                grid[y * n + x] = if (matrix!![x, y]) 1 else 0
            }
        }

        roundMask = API.computeQrMask(grid, n)

        loading = false
        invalidate()
    }

    private var radii = FloatArray(8)
    private val path = Path()
    private val rect = RectF()

    override fun onDraw(canvas: Canvas) {
        if (loading) return
        val mtx = matrix ?: return
        val roundMask = roundMask ?: return

        val sizePx = width.toFloat()
        val m = sizePx / matrixSize
        val radius = m * 0.45f   // rounded data modules

        paint.color = color

        for (y in 0 until matrixSize) {
            for (x in 0 until matrixSize) {
                val i = y * matrixSize + x
                if (!mtx[x, y] || isFinder(x, y, matrixSize)) continue

                val mask = roundMask[i].toInt()

                // TL, TR, BR, BL
                radii[0] = if (mask and 0b0001 != 0) radius else 0f
                radii[1] = radii[0]
                radii[2] = if (mask and 0b0010 != 0) radius else 0f
                radii[3] = radii[2]
                radii[4] = if (mask and 0b0100 != 0) radius else 0f
                radii[5] = radii[4]
                radii[6] = if (mask and 0b1000 != 0) radius else 0f
                radii[7] = radii[6]

                val l = x * m
                val t = y * m

                val eps = 0.5f

                rect.set(
                    l - eps, t - eps, l + m + eps, t + m + eps
                )

                path.reset()
                path.addRoundRect(rect, radii, Path.Direction.CW)

                canvas.drawPath(path, paint)
            }
        }

        // ---- FINDERS (Telegram-style) ----
        val outer = 7 * m
        val innerPad = 1 * m
        val dotPad = 2 * m
        val rOuter = outer * 0.25f
        val rInner = outer * 0.15f

        fun drawFinder(x: Float, y: Float) {
            val l = x
            val t = y
            val rgt = x + outer
            val btm = y + outer

            canvas.saveLayer(l, t, rgt, btm, null)

            // outer
            canvas.drawRoundRect(l, t, rgt, btm, rOuter, rOuter, paint)

            // white ring
            paint.xfermode = xfermode
            canvas.drawRoundRect(
                l + innerPad, t + innerPad, rgt - innerPad, btm - innerPad, rInner, rInner, paint
            )
            paint.xfermode = null

            // center dot (3x3)
            canvas.drawRoundRect(
                l + dotPad,
                t + dotPad,
                rgt - dotPad,
                btm - dotPad,
                rInner * 0.8f,
                rInner * 0.8f,
                paint
            )

            canvas.restore()
        }

        drawFinder(0f, 0f)                     // TL
        drawFinder(sizePx - outer, 0f)         // TR
        drawFinder(0f, sizePx - outer)         // BL
    }

    private lateinit var gradient: Shader

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        gradient = LinearGradient(
            0f, 0f,
            w.toFloat(), h.toFloat(),
            intArrayOf(
                0xFF3D7AB9.toInt(),
                0xFF2868AA.toInt()
            ),
            null,
            Shader.TileMode.CLAMP
        )

        paint.shader = gradient
        regenerate()
    }
}


