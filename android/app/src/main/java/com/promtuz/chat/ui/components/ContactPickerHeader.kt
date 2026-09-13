package com.promtuz.chat.ui.components

import androidx.compose.animation.*
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.Canvas
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.draw.clipToBounds
import com.promtuz.chat.ui.constants.Tweens
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.ui.stage.ChatMotion

/** Search and selection replace the title in place; the screen owns Back precedence. */
@Composable
fun ContactPickerHeader(
    title: String,
    searching: Boolean = false,
    query: String = "",
    onQuery: (String) -> Unit = {},
    close: Boolean,
    onBack: () -> Unit,
    onSearch: (() -> Unit)? = null,
    enabled: Boolean = true,
    selectionCount: Int? = null,
    actions: @Composable RowScope.() -> Unit = {},
) {
    val colors = MaterialTheme.colorScheme
    val keyboard = LocalSoftwareKeyboardController.current
    val focusManager = LocalFocusManager.current
    PickerSearchBackHandler(searching) { if (enabled) onBack() }
    LaunchedEffect(searching) {
        if (!searching) { keyboard?.hide(); focusManager.clearFocus() }
    }
    val morph by animateFloatAsState(if (close || searching) 1f else 0f,
        ChatMotion.spec(), label = "picker back morph")
    Row(Modifier.fillMaxWidth().height(64.dp).padding(horizontal = 4.dp), verticalAlignment = Alignment.CenterVertically) {
        IconButton(onClick = onBack, enabled = enabled) {
            val description = if (searching) "Close search" else if (close) "Clear selection" else "Back"
            val tint = colors.onSurface.copy(alpha = if (enabled) 1f else 0.38f)
            // Interpolate the existing back/close silhouettes with rounded strokes.
            // The navigation button keeps the same bounds throughout the transition.
            Canvas(Modifier.size(24.dp).semantics { contentDescription = description }) {
                fun point(x: Float, y: Float) = Offset(x * size.width / 24f, y * size.height / 24f)
                fun mix(a: Float, b: Float) = a + (b - a) * morph
                val stroke = size.width / 12f
                drawLine(tint, point(mix(5f, 6f), mix(12f, 6f)),
                    point(mix(12f, 18f), mix(5f, 18f)), stroke, StrokeCap.Round)
                drawLine(tint, point(mix(5f, 6f), mix(12f, 18f)),
                    point(mix(12f, 18f), mix(19f, 6f)), stroke, StrokeCap.Round)
                drawLine(tint.copy(alpha = tint.alpha * (1f - morph)), point(5f, 12f),
                    point(mix(19f, 5f), 12f), stroke, StrokeCap.Round)
            }
        }
        AnimatedContent(searching, Modifier.weight(1f), transitionSpec = {
            (fadeIn(ChatMotion.spec()) + slideInHorizontally(ChatMotion.spec()) { it / 8 }) togetherWith
                (fadeOut(ChatMotion.spec()) + slideOutHorizontally(ChatMotion.spec()) { -it / 8 })
        }, label = "picker search") { search ->
            if (search) {
                val focus = remember { FocusRequester() }
                LaunchedEffect(Unit) { focus.requestFocus() }
                BasicTextField(query, onQuery, enabled = enabled, singleLine = true,
                    modifier = Modifier.fillMaxWidth().padding(vertical = 12.dp).focusRequester(focus).semantics { contentDescription = "Search contacts" },
                    textStyle = MaterialTheme.typography.titleMedium.copy(color = colors.onSurface),
                    cursorBrush = SolidColor(colors.primary),
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
                    keyboardActions = KeyboardActions(onSearch = { keyboard?.hide() }),
                    decorationBox = { inner ->
                        Box { if (query.isEmpty()) Text("Search contacts", color = colors.onSurfaceVariant,
                            style = MaterialTheme.typography.titleMedium); inner() }
                    })
            } else AnimatedContent(selectionCount != null, Modifier.fillMaxWidth().clipToBounds(),
                contentAlignment = Alignment.CenterStart, transitionSpec = {
                    ((fadeIn(Tweens.microInteraction(300)) + slideInVertically(Tweens.microInteraction(300)) { it }) togetherWith
                        (fadeOut(Tweens.microInteraction(300)) + slideOutVertically(Tweens.microInteraction(300)) { -it }))
                        .using(null)
                }, label = "picker title mode") { counted ->
                if (counted) {
                    // Retain the outgoing count while selection mode animates away.
                    var lastCount by remember { mutableIntStateOf(selectionCount ?: 1) }
                    if (selectionCount != null) SideEffect { lastCount = selectionCount }
                    Row(verticalAlignment = Alignment.CenterVertically,
                        modifier = Modifier.semantics(mergeDescendants = true) {}) {
                        AnimatedContent(selectionCount ?: lastCount, transitionSpec = {
                            (fadeIn(Tweens.microInteraction(300)) + slideInVertically(Tweens.microInteraction(300)) { it }) togetherWith
                                (fadeOut(Tweens.microInteraction(300)) + slideOutVertically(Tweens.microInteraction(300)) { -it })
                        }, label = "selected count") { Text("$it", style = MaterialTheme.typography.titleLarge) }
                        Text(" selected", style = MaterialTheme.typography.titleLarge)
                    }
                } else Text(title, style = MaterialTheme.typography.titleLarge, maxLines = 1)
            }

        }
        AnimatedVisibility(!searching, enter = fadeIn(ChatMotion.spec()) + expandHorizontally(ChatMotion.spec()),
            exit = fadeOut(ChatMotion.spec()) + shrinkHorizontally(ChatMotion.spec())) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                actions()
                if (onSearch != null) IconButton(onClick = onSearch, enabled = enabled && !searching) {
                    Icon(painterResource(R.drawable.oi_search), "Search contacts", Modifier.size(22.dp))
                }
            }
        }
    }
}
