package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Promtuz

import androidx.compose.animation.*
import androidx.compose.foundation.layout.*
import androidx.compose.ui.draw.clipToBounds
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.ui.animation.QueuedAnimatedNumber
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.listSaver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
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
fun SearchableHeader(
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
    scrollBehavior: TopAppBarScrollBehavior? = null,
    windowInsets: WindowInsets = WindowInsets(0),
    topBarColors: TopAppBarColors = appTopBarColors(),
    showActionsWhileSearching: Boolean = false,
    fade: Boolean = false,
    actions: @Composable RowScope.() -> Unit = {},
) {
    val titleStyle = screenTitleStyle()
    val keyboard = LocalSoftwareKeyboardController.current
    val focusManager = LocalFocusManager.current
    SearchBackHandler(searching) { if (enabled) onBack() }
    LaunchedEffect(searching) {
        if (!searching) { keyboard?.hide(); focusManager.clearFocus() }
    }
    AppTopBar(
        title = {
            AnimatedContent(searching, Modifier.fillMaxWidth().height(48.dp), contentAlignment = Alignment.CenterStart, transitionSpec = {
                (fadeIn(ChatMotion.spec()) togetherWith fadeOut(ChatMotion.spec())).using(null)
            }, label = "header search") { search ->
                if (search) {
                    val focus = remember { FocusRequester() }
                    LaunchedEffect(searching) { if (searching) focus.requestFocus() }
                    BasicTextField(query, onQuery, enabled = enabled && searching, singleLine = true,
                        modifier = Modifier.fillMaxSize().focusRequester(focus).semantics { contentDescription = searchLabel },
                        textStyle = searchTextStyle().copy(color = Promtuz.colors.text),
                        cursorBrush = SolidColor(Promtuz.colors.accentFill),
                        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
                        keyboardActions = KeyboardActions(onSearch = { keyboard?.hide() }),
                        decorationBox = { inner ->
                            Box(Modifier.fillMaxSize(), contentAlignment = Alignment.CenterStart) {
                                if (query.isEmpty()) Text(searchLabel, color = Promtuz.colors.muted,
                                    style = searchTextStyle(), maxLines = 1, overflow = TextOverflow.Ellipsis)
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
                        }, label = "header title mode") { counted ->
                        if (counted) {
                            // Retain the outgoing count while selection mode animates away.
                            var lastCount by remember { mutableIntStateOf(selectionCount ?: 1) }
                            if (selectionCount != null) SideEffect { lastCount = selectionCount }
                            Row(verticalAlignment = Alignment.CenterVertically,
                                modifier = Modifier.semantics(mergeDescendants = true) {}) {
                                QueuedAnimatedNumber(selectionCount ?: lastCount,
                                    label = "selected count", style = titleStyle)
                                Text(" selected", style = titleStyle)
                            }
                        } else if (close) Text(title, style = titleStyle, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        else ConnectionAwareTitle {
                            Text(title, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                    }
                }

            }
        },
        navigationIcon = {
            IconButton(onClick = onBack, enabled = enabled) {
                val description = if (searching) "Close search" else if (close) "Clear selection" else "Back"
                val tint = Promtuz.colors.text.copy(alpha = if (enabled) 1f else 0.38f)
                TopBarMorphIcon(if (close || searching) MorphGlyph.Close else MorphGlyph.Back,
                    description, tint = tint)
            }
        },
        scrollBehavior = scrollBehavior,
        windowInsets = windowInsets,
        colors = topBarColors,
        connectionStatus = false,
        fade = fade,
        actions = {
            AnimatedVisibility(!searching, enter = fadeIn(ChatMotion.spec()) + expandHorizontally(ChatMotion.spec()),
                exit = fadeOut(ChatMotion.spec()) + shrinkHorizontally(ChatMotion.spec())) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (onSearch != null) IconButton(onClick = onSearch, enabled = enabled && !searching) {
                        TopBarIcon(R.drawable.oi_search, searchLabel)
                    }
                    if (!showActionsWhileSearching) actions()
                }
            }
            if (showActionsWhileSearching) actions()
        },
    )
}

@Stable
class ScreenSearch(active: Boolean = false, query: String = "") {
    var active by mutableStateOf(active)
    var query by mutableStateOf(query)
    fun close() { active = false; query = "" }

    companion object {
        val Saver = listSaver<ScreenSearch, Any>({ listOf(it.active, it.query) }, { ScreenSearch(it[0] as Boolean, it[1] as String) })
    }
}

@Composable
fun rememberScreenSearch() = rememberSaveable(saver = ScreenSearch.Saver) { ScreenSearch() }

/** A screen's bar with a search action; Back closes the search before it leaves the screen. */
@Composable
fun SearchTopBar(
    title: String,
    search: ScreenSearch,
    searchLabel: String,
    scrollBehavior: TopAppBarScrollBehavior,
    enabled: Boolean = true,
    fade: Boolean = false,
    showActionsWhileSearching: Boolean = false,
    actions: @Composable RowScope.() -> Unit = {},
) {
    val back = LocalOnBackPressedDispatcherOwner.current?.onBackPressedDispatcher
    SearchableHeader(
        title = title, searching = search.active, query = search.query, onQuery = { search.query = it },
        close = false, enabled = enabled, searchLabel = searchLabel,
        onBack = { if (search.active) search.close() else back?.onBackPressed() },
        onSearch = { search.active = true },
        scrollBehavior = scrollBehavior, windowInsets = TopAppBarDefaults.windowInsets,
        fade = fade, showActionsWhileSearching = showActionsWhileSearching, actions = actions,
    )
}
