package com.promtuz.chat.ui.text

import android.graphics.Typeface
import android.net.Uri
import android.view.inputmethod.InputMethodManager
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Stable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFontFamilyResolver
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontSynthesis
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.isSpecified
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView

/** The composer owns keyboard requests; native editors have their own IME connection. */
@Stable
class EmojiFieldController {
    internal var editor: EmojiEditText? = null

    fun showKeyboard() {
        editor?.let { view ->
            view.requestFocus()
            view.post {
                if (view.isAttachedToWindow && view.hasFocus()) {
                    view.context.getSystemService(InputMethodManager::class.java)
                        .showSoftInput(view, InputMethodManager.SHOW_IMPLICIT)
                }
            }
        }
    }

    fun hideKeyboard() {
        editor?.let { view ->
            view.context.getSystemService(InputMethodManager::class.java)
                .hideSoftInputFromWindow(view.windowToken, 0)
        }
    }

    fun clearFocus() {
        hideKeyboard()
        editor?.clearFocus()
    }
}

/** Native editing and span measurement, with the same pack used by [EmojiText]. */
@Composable
fun EmojiTextField(
    value: String,
    onValueChange: (String) -> Unit,
    modifier: Modifier = Modifier,
    textStyle: TextStyle,
    cursorColor: Color,
    maxLines: Int,
    controller: EmojiFieldController,
    acceptChanges: Boolean = true,
    onFieldFocused: () -> Unit = {},
    onReceiveImages: (List<Uri>) -> Unit = {},
    decorationBox: @Composable (@Composable () -> Unit) -> Unit,
) {
    EmojiPack.ensureLoaded(LocalContext.current)
    val index by EmojiPack.index.collectAsState()
    val scope = rememberCoroutineScope()
    val sync = remember { EmojiInputSync(value) }
    val density = LocalDensity.current
    val fontSize = with(density) { (if (textStyle.fontSize.isSpecified) textStyle.fontSize else 16.sp).toPx() }
    val lineHeight = with(density) { if (textStyle.lineHeight.isSpecified) textStyle.lineHeight.toPx() else 0f }
    val tracking = when {
        textStyle.letterSpacing.isEm -> textStyle.letterSpacing.value
        textStyle.letterSpacing.isSp -> with(density) { textStyle.letterSpacing.toPx() / fontSize }
        else -> 0f
    }
    val typeface by LocalFontFamilyResolver.current.resolve(
        textStyle.fontFamily,
        textStyle.fontWeight ?: FontWeight.Normal,
        textStyle.fontStyle ?: FontStyle.Normal,
        textStyle.fontSynthesis ?: FontSynthesis.All,
    )
    Box(modifier) {
        decorationBox {
            AndroidView(
                modifier = Modifier.fillMaxWidth(),
                factory = { context -> EmojiEditText(context, scope).also { controller.editor = it } },
                onRelease = { view ->
                    if (controller.editor === view) controller.editor = null
                    view.release()
                },
                update = { view ->
                    view.acceptChanges = acceptChanges
                    view.onEdited = { text -> sync.publish(text); onValueChange(text) }
                    view.onFieldFocused = onFieldFocused
                    view.onReceiveImages = onReceiveImages
                    view.updateStyle(typeface as Typeface, fontSize, lineHeight, tracking,
                        textStyle.color.toArgb(), cursorColor.toArgb(), maxLines)
                    view.updateIndex(index)
                    if (sync.acceptExternal(value)) view.setDraft(value)
                },
            )
        }
    }
}
