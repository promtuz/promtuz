package com.promtuz.chat.ui.components

import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.animation.core.animateDpAsState
import com.promtuz.chat.ui.stage.ChatMotion
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.input.ImeAction
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.ChatVM
import org.koin.compose.koinInject
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.chatBarHaze
import com.promtuz.chat.ui.util.freezeOnExit
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatTopBar(name: String, chatVM: ChatVM, haze: HazeState) {
    val appVM = koinInject<AppVM>()
    val navigator = appVM.navigator
    val backHandle = LocalOnBackPressedDispatcherOwner.current
    val colors = MaterialTheme.colorScheme
    val chatTheme = LocalChatColors.current
    val typing by chatVM.typing.collectAsState()
    val presence by chatVM.presence.collectAsState()
    val isGroup by chatVM.isGroup.collectAsState()
    val memberNames by chatVM.memberNames.collectAsState()
    val memberCount by chatVM.memberCount.collectAsState()
    val rawTitle by chatVM.rawTitle.collectAsState()
    val muted by chatVM.muted.collectAsState()
    val typingMembers by chatVM.typingMembers.collectAsState()

    // Delete needs the same standing the home list uses to decide what to offer
    // (can we leave, did we found it, are we still a member), and that already
    // lives on the home summaries — one source, not a second read of our own.
    // The lookup is derived so a message in some other chat doesn't recompose
    // this bar, and keyed on the id because that is a plain getter rather than
    // a State the derivation could re-read.
    val chats by appVM.chats.collectAsState()
    val summary by remember(chatVM.conversationHex) {
        derivedStateOf { chats.firstOrNull { it.conversationHex == chatVM.conversationHex } }
    }
    var confirmClear by remember { mutableStateOf(false) }
    var confirmDelete by remember { mutableStateOf(false) }

    val searchQuery by chatVM.searchQuery.collectAsState()
    val searching = searchQuery != null
    BackHandler(searching) { chatVM.closeSearch() }
    val navigationMorph = rememberMorphIconState(if (searching) MorphGlyph.Close else MorphGlyph.ChevronLeft)
    // Hoisted above the two toolbar modes so switching layouts retains the motion.
    val navigationWidth by animateDpAsState(if (searching) 40.dp else 24.dp,
        ChatMotion.spec(), label = "chat navigation width")
    val navigationRadius by animateDpAsState(if (searching) 20.dp else 8.dp,
        ChatMotion.spec(), label = "chat navigation corner")
    val navigationIcon: @Composable () -> Unit = {
        Box(
            Modifier.padding(start = 6.dp).width(navigationWidth).height(40.dp)
                .clip(RoundedCornerShape(navigationRadius))
                .clickable {
                    if (searching) chatVM.closeSearch() else backHandle?.onBackPressedDispatcher?.onBackPressed()
                },
            contentAlignment = Alignment.Center,
        ) {
            MorphIcon(navigationMorph, if (searching) "Close search" else "Back", Modifier.size(24.dp))
        }
    }

    // The summaries come back empty on a transient FFI failure, which hides the
    // delete dialog without answering it; a flag left standing would raise it
    // again unasked once the list recovers.
    LaunchedEffect(summary == null) { if (summary == null) confirmDelete = false }

    // Who's typing, named — a group can have several at once, and "3 people
    // typing…" reads better than three names past a couple.
    val typingLine = remember(typingMembers, memberNames) {
        val names = typingMembers.mapNotNull { memberNames[it] }
        when {
            names.isEmpty() -> "typing…"
            names.size == 1 -> "${names[0]} is typing…"
            names.size == 2 -> "${names[0]} and ${names[1]} are typing…"
            else -> "${names.size} people are typing…"
        }
    }

    val presenceNow = rememberPresenceTime()

    // Subtitle cascade: live activity beats presence; silence renders nothing.
    // A group has no single presence, so it falls back to its member count.
    val (subtitle, subtitleColor) = when {
        typing && isGroup -> typingLine to chatTheme.accent
        typing -> "typing…" to chatTheme.accent
        isGroup -> memberTally(memberCount) to colors.onSurfaceVariant
        else -> presenceText(presence, presenceNow) to
            if (presence == Presence.Online) chatTheme.accent else colors.onSurfaceVariant
    }

    TopAppBar(
        title = {
            if (searching) SearchField(chatVM, searchQuery.orEmpty()) else Row(
                modifier = Modifier.clickable(enabled = isGroup) { navigator.push(Routes.GroupInfo(chatVM.conversationHex)) },
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                if (isGroup) GroupAvatar(title = rawTitle, members = memberNames.values.toList(), size = 40.dp)
                else Avatar(name, 40.dp)
                Column {
                    Text(name, style = MaterialTheme.typography.titleMediumEmphasized, maxLines = 1)
                    if (subtitle != null) Text(
                        subtitle,
                        style = MaterialTheme.typography.labelMedium,
                        color = subtitleColor,
                    )
                }
            }
        },
        navigationIcon = navigationIcon,
        actions = {
            if (searching) SearchActions(chatVM, searchQuery.orEmpty()) else AppDropMenu(
                iconSize = 20.dp,
                anchor = { DrawableIcon(R.drawable.i_ellipsis_vertical, Modifier.padding(12.dp), desc = "Chat options") },
                groups = buildList {
                    if (isGroup) {
                        add(
                            listOf(
                                MenuAction("Group info", R.drawable.i_contacts) {
                                    navigator.push(Routes.GroupInfo(chatVM.conversationHex))
                                },
                            ),
                        )
                    }
                    add(
                        listOf(
                            MenuAction("Search", R.drawable.oi_search) { chatVM.openSearch() },
                            MenuAction(if (muted) "Unmute" else "Mute", if (muted) R.drawable.oi_bell_on else R.drawable.oi_bell_slash) {
                                chatVM.toggleMute()
                            })
                    )
                    add(
                        buildList {
                            add(
                                MenuAction("Clear History", R.drawable.oi_clear_list) {
                                    confirmClear = true
                                },
                            )
                            // The dialog needs the summary to know what it is
                            // deleting; while it hasn't arrived there is nothing
                            // honest to offer, so offer nothing.
                            if (summary != null) add(
                                MenuAction("Delete Chat", R.drawable.oi_trash, destructive = true) {
                                    confirmDelete = true
                                },
                            )
                        },
                    )
                },
            )
        },
        // freezeOnExit: bake the blur to pixels while the nav card scales out (Haze
        // samples screen-space and shatters under an ancestor scale).
        modifier = Modifier
            .freezeOnExit()
            .hazeEffect(haze, chatBarHaze()),
        colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
    )

    if (confirmClear) ClearHistoryDialog(
        name = name,
        onConfirm = { confirmClear = false; appVM.clearHistory(chatVM.conversationHex) },
        onDismiss = { confirmClear = false },
    )
    // Both paths take the chat we are reading out from under us, so the step
    // back to the list waits on the work landing: a leave that fails keeps the
    // chat, and the screen showing it is where the user should still be.
    //
    // That wait runs on AppVM, a Koin `single`, so it outlives this screen —
    // leaving needs the network and deleting needs the DB, and the user can be
    // in Settings by the time either lands. Step back only while this chat is
    // still what's on top, or the pop lands on whatever they moved to.
    val popThisChat = {
        val top = navigator.backStack.lastOrNull()
        if ((top as? Routes.Chat)?.conversation == chatVM.conversationHex) navigator.back()
    }
    summary?.let { chat ->
        if (confirmDelete) DeleteChatDialog(
            chat = chat,
            onDelete = { confirmDelete = false; appVM.deleteChat(chat, popThisChat) },
            onLeaveAndDelete = { confirmDelete = false; appVM.leaveAndDelete(chat, popThisChat) },
            onDismiss = { confirmDelete = false },
        )
    }
}

/** "1 member" / "4 members" — a group of one is a real state after a removal. */
fun memberTally(n: Int): String = if (n == 1) "1 member" else "$n members"

/**
 * The top bar while searching: the field where the name was, and the walk
 * through the hits where the menu was. Hits are counted newest first, so
 * "up" goes further back — the direction the thumb expects in a chat that
 * grows downward.
 */
@Composable
private fun SearchField(chatVM: ChatVM, query: String) {
    val colors = MaterialTheme.colorScheme
    val chatTheme = LocalChatColors.current
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
            BasicTextField(
                value = query,
                onValueChange = { chatVM.searchQuery.value = it },
                singleLine = true,
                textStyle = MaterialTheme.typography.bodyLarge.copy(color = colors.onSurface),
                cursorBrush = SolidColor(chatTheme.accent),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
                keyboardActions = KeyboardActions(onSearch = { chatVM.nextHit() }),
                modifier = Modifier.fillMaxWidth().focusRequester(focus).semantics { contentDescription = "Search messages" },
                decorationBox = { inner ->
                    Box {
                        if (query.isEmpty()) Text(
                            "Search",
                            style = MaterialTheme.typography.bodyLarge,
                            color = colors.onSurfaceVariant,
                        )
                        inner()
                    }
                },
            )
}

@Composable
private fun SearchActions(chatVM: ChatVM, query: String) {
    val colors = MaterialTheme.colorScheme
    val hits by chatVM.hits.collectAsState()
    val index by chatVM.hitIndex.collectAsState()
    val count = when {
        query.isBlank() -> ""
        hits.isEmpty() -> "0"
        else -> "${index + 1}/${hits.size}"
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                count,
                style = MaterialTheme.typography.labelMedium,
                color = colors.onSurfaceVariant,
                modifier = Modifier.padding(end = 4.dp),
            )
            val enabled = hits.size > 1
            Box(
                Modifier.size(40.dp).clip(CircleShape).clickable(enabled = enabled) { chatVM.nextHit() },
                contentAlignment = Alignment.Center,
            ) {
                DrawableIcon(
                    R.drawable.i_back_chevron, Modifier.size(18.dp).rotate(90f),
                    tint = if (enabled) colors.onSurface else colors.onSurfaceVariant.copy(alpha = 0.4f),
                )
            }
            Box(
                Modifier.padding(end = 6.dp).size(40.dp).clip(CircleShape)
                    .clickable(enabled = enabled) { chatVM.prevHit() },
                contentAlignment = Alignment.Center,
            ) {
                DrawableIcon(
                    R.drawable.i_back_chevron, Modifier.size(18.dp).rotate(-90f),
                    tint = if (enabled) colors.onSurface else colors.onSurfaceVariant.copy(alpha = 0.4f),
                )
            }
    }
}
