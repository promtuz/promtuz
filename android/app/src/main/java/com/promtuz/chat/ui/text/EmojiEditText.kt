package com.promtuz.chat.ui.text

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.RectF
import android.graphics.Typeface
import android.net.Uri
import android.os.Build
import android.text.InputFilter
import android.text.InputType
import android.text.Spanned
import android.text.TextWatcher
import android.text.Editable
import android.text.style.ReplacementSpan
import android.util.TypedValue
import android.view.Gravity
import android.view.MotionEvent
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputConnectionWrapper
import androidx.appcompat.widget.AppCompatEditText
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.core.view.ViewCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlin.math.ceil
import kotlin.math.roundToInt

/** Keep one Editable and IME connection; only our rendering spans are replaced after edits. */
internal class EmojiEditText(context: Context, private val scope: CoroutineScope) : AppCompatEditText(context) {
    var acceptChanges = true
    var onEdited: (String) -> Unit = {}
    var onFieldFocused: () -> Unit = {}
    var onReceiveImages: (List<Uri>) -> Unit = {}
    private var applyingDraft = false
    private var released = false
    private var index: EmojiPack.Index? = null
    private val loads = HashMap<String, Job>()
    private val failed = HashSet<String>()
    private var styledLineHeight = -1f
    private var styledCursorColor: Int? = null

    init {
        // AppCompat supplies receive-content / IME support across our API range.
        // Its system EmojiCompat spans must not compete with the bundled pack.
        isEmojiCompatEnabled = false
        background = null
        setPadding(0, 0, 0, 0)
        minWidth = 0
        minimumWidth = 0
        minHeight = 0
        minimumHeight = 0
        includeFontPadding = false
        gravity = Gravity.START or Gravity.CENTER_VERTICAL
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE or
            InputType.TYPE_TEXT_FLAG_CAP_SENTENCES or InputType.TYPE_TEXT_FLAG_AUTO_CORRECT
        imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN
        setHorizontallyScrolling(false)
        isVerticalScrollBarEnabled = false
        // Draft text comes from the VM. Replaying view hierarchy text would create another owner.
        isSaveEnabled = false
        contentDescription = "Message input"
        filters = arrayOf(InputFilter { _, _, _, dest, start, end ->
            if (acceptChanges || applyingDraft) null else dest.subSequence(start, end)
        })
        addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) = Unit
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) = Unit
            override fun afterTextChanged(text: Editable) {
                updateSpans(text)
                if (!applyingDraft) onEdited(text.toString())
            }
        })
        setOnFocusChangeListener { _, focused -> if (focused) onFieldFocused() }
        ViewCompat.setOnReceiveContentListener(this, arrayOf("image/*")) { _, payload ->
            if (!acceptChanges || !payload.clip.description.hasMimeType("image/*")) payload
            else {
                val parts = payload.partition { it.uri != null }
                val images = parts.first
                if (images != null) onReceiveImages((0 until images.clip.itemCount).map { images.clip.getItemAt(it).uri })
                parts.second
            }
        }
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        // Also handles tapping an already-focused field while an accessory panel is open.
        if (event.actionMasked == MotionEvent.ACTION_DOWN) onFieldFocused()
        return super.onTouchEvent(event)
    }

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        val connection = super.onCreateInputConnection(outAttrs) ?: return null
        return object : InputConnectionWrapper(connection, false) {
            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean =
                deleteEmoji(beforeLength, afterLength, false) || super.deleteSurroundingText(beforeLength, afterLength)

            override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean =
                deleteEmoji(beforeLength, afterLength, true) || super.deleteSurroundingTextInCodePoints(beforeLength, afterLength)
        }
    }

    private fun deleteEmoji(before: Int, after: Int, codePoints: Boolean): Boolean {
        val editable = text ?: return false
        val cursor = selectionStart
        if (before < 0 || after < 0 || cursor < 0 || cursor != selectionEnd) return false
        var start = if (codePoints) Character.offsetByCodePoints(editable, cursor,
            -minOf(before, Character.codePointCount(editable, 0, cursor))) else cursor - minOf(before, cursor)
        var end = if (codePoints) Character.offsetByCodePoints(editable, cursor,
            minOf(after, Character.codePointCount(editable, cursor, editable.length)))
            else cursor + minOf(after, editable.length - cursor)
        if (start == end) return false
        val spans = editable.getSpans(start, end, PackEmojiSpan::class.java).filter {
            editable.getSpanStart(it) < end && editable.getSpanEnd(it) > start
        }
        if (spans.isEmpty()) return false
        // Some keyboards delete UTF-16 units, others code points. Both must delete
        // the entire displayed emoji rather than leaving a broken surrogate/ZWJ sequence.
        for (span in spans) {
            start = minOf(start, editable.getSpanStart(span))
            end = maxOf(end, editable.getSpanEnd(span))
        }
        beginBatchEdit()
        try {
            editable.delete(start, end)
        } finally {
            endBatchEdit()
        }
        return true
    }

    fun setDraft(value: String) {
        val editable = text ?: return
        if (editable.toString() == value) return
        applyingDraft = true
        try {
            beginBatchEdit()
            BaseInputConnection.removeComposingSpans(editable)
            editable.replace(0, editable.length, value)
            setSelection(editable.length)
        } finally {
            endBatchEdit()
            applyingDraft = false
        }
    }

    fun updateIndex(next: EmojiPack.Index?) {
        if (index === next) return
        index = next
        text?.let(::updateSpans)
    }

    fun updateStyle(face: Typeface, size: Float, lineHeight: Float, tracking: Float,
                    color: Int, accent: Int, lines: Int) {
        val fontChanged = typeface != face || textSize != size
        if (typeface != face) typeface = face
        if (textSize != size) setTextSize(TypedValue.COMPLEX_UNIT_PX, size)
        if (letterSpacing != tracking) letterSpacing = tracking
        if (currentTextColor != color) setTextColor(color)
        if (maxLines != lines) maxLines = lines
        if (fontChanged || styledLineHeight != lineHeight) {
            styledLineHeight = lineHeight
            setLineSpacing((lineHeight - paint.fontMetricsInt.let { it.descent - it.ascent }).coerceAtLeast(0f), 1f)
            minimumHeight = ceil(lineHeight).toInt()
        }
        if (styledCursorColor != accent) {
            styledCursorColor = accent
            highlightColor = (accent and 0x00ffffff) or 0x55000000
            // Older Android versions retain their native theme's cursor/handle colours.
            if (Build.VERSION.SDK_INT >= 29) {
                textCursorDrawable = textCursorDrawable?.mutate()?.apply { setTint(accent) }
                textSelectHandle?.mutate()?.let { it.setTint(accent); setTextSelectHandle(it) }
                textSelectHandleLeft?.mutate()?.let { it.setTint(accent); setTextSelectHandleLeft(it) }
                textSelectHandleRight?.mutate()?.let { it.setTint(accent); setTextSelectHandleRight(it) }
            }
        }
    }

    private fun updateSpans(editable: Editable) {
        val pack = index ?: return
        // Preserve composing, suggestion and selection spans owned by the editor/IME.
        editable.getSpans(0, editable.length, PackEmojiSpan::class.java).forEach(editable::removeSpan)
        var offset = 0
        val needed = HashSet<String>()
        for (run in EmojiSequences.split(editable.toString(), pack.keys, pack.aliases)) {
            when (run) {
                is EmojiRun.Text -> offset += run.text.length
                is EmojiRun.Emoji -> {
                    val bitmap = EmojiPack.peek(run.key)?.asAndroidBitmap()
                    editable.setSpan(PackEmojiSpan(run.key, bitmap), offset, offset + run.cluster.length,
                        Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
                    offset += run.cluster.length
                    if (bitmap == null) needed += run.key
                }
            }
        }
        loads.keys.toList().filter { it !in needed }.forEach { loads.remove(it)?.cancel() }
        for (key in needed) if (key !in loads && key !in failed) {
            loads[key] = scope.launch {
                val bitmap = EmojiPack.glyph(context, key)?.asAndroidBitmap()
                if (!released) {
                    if (bitmap == null) failed += key
                    text?.let { editable ->
                        editable.getSpans(0, editable.length, PackEmojiSpan::class.java).forEach {
                            if (it.assetKey == key) it.bitmap = bitmap
                        }
                    }
                    // The span already reserved its final size; loading only changes pixels.
                    invalidate()
                    loads.remove(key)
                }
            }
        }
    }

    fun release() {
        released = true
        loads.values.forEach { it.cancel() }
        loads.clear()
        onEdited = {}
        onFieldFocused = {}
        onReceiveImages = {}
        ViewCompat.setOnReceiveContentListener(this, null, null)
    }
}

/** Measurement belongs to Android's text layout, including wrapping, hit testing and scrolling. */
private class PackEmojiSpan(val assetKey: String, var bitmap: Bitmap?) : ReplacementSpan() {
    private val bitmapPaint = Paint(Paint.ANTI_ALIAS_FLAG or Paint.FILTER_BITMAP_FLAG)
    private val bounds = RectF()
    private fun size(paint: Paint) = (paint.textSize * EmojiSizeEm).roundToInt()

    override fun getSize(paint: Paint, text: CharSequence, start: Int, end: Int, fm: Paint.FontMetricsInt?): Int {
        val edge = size(paint)
        if (fm != null) {
            paint.getFontMetricsInt(fm)
            val extra = (edge - (fm.descent - fm.ascent)).coerceAtLeast(0)
            fm.ascent -= extra / 2
            fm.descent += extra - extra / 2
            fm.top = minOf(fm.top, fm.ascent)
            fm.bottom = maxOf(fm.bottom, fm.descent)
        }
        return edge
    }

    override fun draw(canvas: Canvas, text: CharSequence, start: Int, end: Int, x: Float,
                      top: Int, y: Int, bottom: Int, paint: Paint) {
        val edge = size(paint).toFloat()
        val centerY = y + (paint.fontMetrics.ascent + paint.fontMetrics.descent) / 2f
        val image = bitmap
        if (image != null) {
            val scale = edge / maxOf(image.width, image.height)
            val w = image.width * scale
            val h = image.height * scale
            bounds.set(x + (edge - w) / 2, centerY - h / 2, x + (edge + w) / 2, centerY + h / 2)
            bitmapPaint.alpha = paint.alpha
            canvas.drawBitmap(image, null, bounds, bitmapPaint)
        } else {
            // Keep the original Unicode visible until decoding completes (or if decoding fails).
            val width = paint.measureText(text, start, end)
            val scale = if (width > 0) minOf(1f, edge / width) else 1f
            canvas.save()
            canvas.translate(x + edge / 2, centerY)
            canvas.scale(scale, scale)
            canvas.drawText(text, start, end, -width / 2, -(paint.fontMetrics.ascent + paint.fontMetrics.descent) / 2, paint)
            canvas.restore()
        }
    }
}
