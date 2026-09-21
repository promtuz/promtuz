package com.promtuz.chat.ui.components

import androidx.compose.animation.*
import androidx.compose.foundation.layout.*
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
import androidx.compose.ui.text.style.TextOverflow
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
    searchLabel: String = "Search contacts",
    enabled: Boolean = true,
    selectionCount: Int? = null,
    actions: @Composable RowScope.() -> Unit = {},
) {
    val colors = MaterialTheme.colorScheme
    val titleStyle = MaterialTheme.typography.titleLarge
    val keyboard = LocalSoftwareKeyboardController.current
    val focusManager = LocalFocusManager.current
    PickerSearchBackHandler(searching) { if (enabled) onBack() }
    LaunchedEffect(searching) {
        if (!searching) { keyboard?.hide(); focusManager.clearFocus() }
    }
    Row(Modifier.fillMaxWidth().height(64.dp).padding(horizontal = 4.dp), verticalAlignment = Alignment.CenterVertically) {
        IconButton(onClick = onBack, enabled = enabled) {
            val description = if (searching) "Close search" else if (close) "Clear selection" else "Back"
            val tint = colors.onSurface.copy(alpha = if (enabled) 1f else 0.38f)
            TopBarMorphIcon(if (close || searching) MorphGlyph.Close else MorphGlyph.Back,
                description, tint = tint)
        }
        AnimatedContent(searching, Modifier.weight(1f).height(48.dp), contentAlignment = Alignment.CenterStart, transitionSpec = {
            (fadeIn(ChatMotion.spec()) togetherWith fadeOut(ChatMotion.spec())).using(null)
        }, label = "picker search") { search ->
            if (search) {
                val focus = remember { FocusRequester() }
                LaunchedEffect(searching) { if (searching) focus.requestFocus() }
                BasicTextField(query, onQuery, enabled = enabled && searching, singleLine = true,
                    modifier = Modifier.fillMaxSize().focusRequester(focus).semantics { contentDescription = searchLabel },
                    textStyle = titleStyle.copy(color = colors.onSurface),
                    cursorBrush = SolidColor(colors.primary),
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
                    keyboardActions = KeyboardActions(onSearch = { keyboard?.hide() }),
                    decorationBox = { inner ->
                        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.CenterStart) {
                            if (query.isEmpty()) Text(searchLabel, color = colors.onSurfaceVariant,
                                style = titleStyle, maxLines = 1, overflow = TextOverflow.Ellipsis)
                            inner()
                        }
                    })
            } else Box(Modifier.fillMaxSize(), contentAlignment = Alignment.CenterStart,
                propagateMinConstraints = false) {
                AnimatedContent(selectionCount != null, Modifier.fillMaxWidth().clipToBounds(),
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
                                ((fadeIn(Tweens.microInteraction(300)) + slideInVertically(Tweens.microInteraction(300)) { it }) togetherWith
                                    (fadeOut(Tweens.microInteraction(300)) + slideOutVertically(Tweens.microInteraction(300)) { -it }))
                                    .using(SizeTransform(clip = false, sizeAnimationSpec = { _, _ -> Tweens.microInteraction(300) }))
                            }, label = "selected count") { Text("$it", style = titleStyle) }
                            Text(" selected", style = titleStyle)
                        }
                    } else Text(title, style = titleStyle, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
            }

        }
        AnimatedVisibility(!searching, enter = fadeIn(ChatMotion.spec()) + expandHorizontally(ChatMotion.spec()),
            exit = fadeOut(ChatMotion.spec()) + shrinkHorizontally(ChatMotion.spec())) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                actions()
                if (onSearch != null) IconButton(onClick = onSearch, enabled = enabled && !searching) {
                    Icon(painterResource(R.drawable.oi_search), searchLabel, Modifier.size(22.dp))
                }
            }
        }
    }
}
