package com.promtuz.chat.ui.screens

import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.animation.QueuedAnimatedNumber
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.spring
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.shrinkHorizontally
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.draw.dropShadow
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.stage.ChatMotion
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.Radii
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import kotlinx.coroutines.launch
import androidx.compose.runtime.saveable.listSaver
import androidx.compose.runtime.saveable.rememberSaveable
import com.promtuz.chat.utils.media.rememberAvatar
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import kotlinx.coroutines.flow.Flow

data class SendTarget(val conversation: String, val name: String, val group: Boolean, val peer: String?, val members: Int)

/** Chats a message can go to now: unlocked, paired, and not read-only. */
fun sendTargets(): Flow<List<SendTarget>> = observeQuery(setOf("contacts", "conversations", "conversation_members")) {
    val contacts = CoreBridge.contacts().associateBy { it.ipk.toHex() }
    val hidden = CoreBridge.privateConversationIds().map { it.toHex() }.toSet()
    CoreBridge.listConversations().filter { c -> !c.locked && !c.request && c.id.toHex() !in hidden }.filter { c ->
        if (c.kind.toInt() == 1) c.canSend else contacts[c.peer?.toHex()]?.status?.toInt() == 1
    }.map { c ->
        val group = c.kind.toInt() == 1
        SendTarget(c.id.toHex(), if (group) c.displayName else contacts[c.peer?.toHex()]?.name.orEmpty(),
            group, c.peer?.toHex(), c.members.size)
    }
}

/** What is being sent, shown on the bar above the comment field. */
class SendToSubject(val headline: String, val tile: @Composable () -> Unit)

/**
 * Picks one or more chats for something that already exists: shared from another app, or forwarded.
 */
@Composable
fun SendToScreen(
    title: String,
    targets: List<SendTarget>,
    selected: List<String>,
    onToggle: (String) -> Unit,
    subject: SendToSubject,
    comment: String,
    onComment: (String) -> Unit,
    commentHint: String,
    canSend: Boolean,
    busy: Boolean,
    error: String?,
    onSend: () -> Unit,
) {
    val c = Promtuz.colors
    val search = rememberScreenSearch()
    val query = search.query
    BackHandler(busy) { }
    val haze = rememberHazeState()
    val density = LocalDensity.current
    var barHeight by remember { mutableStateOf(0.dp) }
    val presence by CoreBridge.presenceByPeer.collectAsState()
    val now = rememberPresenceTime()
    val visible = remember(targets, query) { targets.filter { it.name.contains(query.trim(), ignoreCase = true) } }

    ScreenScaffold(topBar = { scrollBehavior ->
        SearchTopBar(title, search, "Search chats", scrollBehavior, enabled = !busy)
    }) { padding ->
        Box(Modifier.fillMaxSize()) {
            LazyColumn(
                Modifier.fillMaxSize().hazeSource(haze),
                contentPadding = PaddingValues(top = padding.calculateTopPadding() + 4.dp, bottom = barHeight + 16.dp),
            ) {
                if (visible.isEmpty()) item {
                    Text(if (query.isBlank()) "No chats yet" else "No chats found", Modifier.padding(24.dp), color = c.muted)
                }
                items(visible, key = { it.conversation }) { t ->
                    val picked = t.conversation in selected
                    val status = if (t.group) memberTally(t.members) else presenceText(presence[t.peer], now)
                    Row(
                        Modifier.animateItem(placementSpec = null).fillMaxWidth()
                            .semantics { this.selected = picked }
                            .clickable(enabled = !busy, role = Role.Checkbox) { onToggle(t.conversation) }
                            .padding(horizontal = 16.dp, vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(14.dp),
                    ) {
                        SelectableAvatar(picked, 56.dp, 52.dp) {
                            if (t.group) GroupAvatar(t.name, emptyList(), conversation = t.conversation)
                            else Avatar(t.name, identityKey = t.peer ?: t.conversation, image = rememberAvatar(t.peer),
                                statusColor = when (presence[t.peer]) {
                                    Presence.Online -> c.success
                                    is Presence.Idle -> c.warn
                                    else -> null
                                }.takeIf { !picked })
                        }
                        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                            Text(t.name, style = RowTitleStyle, fontWeight = FontWeight.SemiBold, color = c.text,
                                maxLines = 1, overflow = TextOverflow.Ellipsis)
                            if (status != null) Text(status, style = RowSubtitleStyle,
                                color = if (presence[t.peer] == Presence.Online) c.accent else c.muted, maxLines = 1)
                        }
                    }
                }
            }
            SendToBar(
                Modifier.align(Alignment.BottomCenter).onSizeChanged { barHeight = with(density) { it.height.toDp() } },
                haze, subject, targets, selected, comment, onComment, commentHint, canSend, busy, error, onSend,
            )
        }
    }
}

@Composable
private fun SendToBar(
    modifier: Modifier,
    haze: dev.chrisbanes.haze.HazeState,
    subject: SendToSubject,
    targets: List<SendTarget>,
    selected: List<String>,
    comment: String,
    onComment: (String) -> Unit,
    commentHint: String,
    canSend: Boolean,
    busy: Boolean,
    error: String?,
    onSend: () -> Unit,
) {
    val c = Promtuz.colors
    val shape = RoundedCornerShape(26.dp)
    val names = selected.mapNotNull { id -> targets.firstOrNull { it.conversation == id }?.name }
    val picked = selected.isNotEmpty()
    CompositionLocalProvider(LocalOnGlass provides true) {
    Column(
        modifier.fillMaxWidth().imePadding().navigationBarsPadding().padding(8.dp)
            .floatingSurface(shape, haze)
            .padding(10.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Row(Modifier.padding(horizontal = 4.dp), verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            subject.tile()
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(1.dp)) {
                Text(subject.headline, fontSize = 14.5.sp, lineHeight = 17.sp, fontWeight = FontWeight.Medium,
                    color = c.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(
                    error ?: if (names.isEmpty()) "Pick who to send it to" else "To ${names.joinToString(", ")}",
                    fontSize = 13.sp, lineHeight = 15.sp,
                    color = when { error != null -> c.danger; names.isEmpty() -> c.muted; else -> c.accent },
                    maxLines = 1, overflow = TextOverflow.Ellipsis,
                )
            }
        }
        // The field and the send button join as a pair: 20 outside, 16 where they meet.
        val joint by androidx.compose.animation.core.animateDpAsState(if (picked) 16.dp else Radii.field,
            ChatMotion.spec(), label = "field joint")
        Row(verticalAlignment = Alignment.CenterVertically) {
            BasicTextField(comment, onComment, enabled = !busy, maxLines = 5,
                textStyle = RowTitleStyle.copy(color = c.text),
                cursorBrush = SolidColor(c.accentFill),
                keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Sentences),
                modifier = Modifier.weight(1f).heightIn(min = 46.dp)
                    .clip(RoundedCornerShape(Radii.field, joint, joint, Radii.field))
                    .background(raisedFill()),
                decorationBox = { inner ->
                    Box(Modifier.padding(horizontal = 18.dp, vertical = 12.dp), contentAlignment = Alignment.CenterStart) {
                        if (comment.isEmpty()) Text(commentHint, style = RowTitleStyle, color = c.muted)
                        inner()
                    }
                })
            // While leaving, the button keeps the look it had: unpicking the last chat must not grey it or drop its count first.
            var shownCount by remember { mutableIntStateOf(selected.size) }
            if (picked) shownCount = selected.size
            AnimatedVisibility(picked,
                enter = fadeIn(ChatMotion.spec()) + scaleIn(spring(), initialScale = 0.5f),
                exit = fadeOut(ChatMotion.spec()) + scaleOut(ChatMotion.spec(), targetScale = 0.5f) +
                    shrinkHorizontally(ChatMotion.spec(), shrinkTowards = Alignment.End, clip = false)) {
                Box(Modifier.padding(start = 6.dp)) {
                    Box(Modifier.size(46.dp).clip(RoundedCornerShape(16.dp, Radii.field, Radii.field, 16.dp))
                        .background(if (busy) raisedFill() else c.accentFill)
                        .clickable(enabled = canSend, role = Role.Button, onClick = onSend),
                        contentAlignment = Alignment.Center) {
                        DrawableIcon(R.drawable.i_send, desc = "Send", size = 22.dp, tint = Color.White)
                    }
                    androidx.compose.animation.AnimatedVisibility(shownCount > 1, Modifier.align(Alignment.TopEnd).offset(x = 5.dp, y = (-5).dp),
                        enter = fadeIn(ChatMotion.spec()) + scaleIn(spring(), initialScale = 0.5f),
                        exit = fadeOut(ChatMotion.spec()) + scaleOut(ChatMotion.spec(), targetScale = 0.5f)) {
                        Box(
                            Modifier.dropShadow(RoundedCornerShape(9.dp), androidx.compose.ui.graphics.shadow.Shadow(3.dp, Color.Black.copy(alpha = 0.25f)))
                                .defaultMinSize(22.dp, 22.dp).clip(RoundedCornerShape(9.dp)).background(Color.White)
                                .padding(horizontal = 6.dp),
                            contentAlignment = Alignment.Center,
                        ) {
                            QueuedAnimatedNumber(shownCount.coerceAtLeast(2),
                                style = androidx.compose.ui.text.TextStyle(fontSize = 12.sp, lineHeight = 14.sp, fontWeight = FontWeight.Medium),
                                color = c.accentFill, label = "send count")
                        }
                    }
                }
            }
        }
    }
    }
}

/** The square a subject sits on: an icon tile, or thumbnails stacked like cards. */
@Composable
fun SubjectIconTile(icon: Int) {
    Box(Modifier.size(36.dp).clip(RoundedCornerShape(10.dp)).background(raisedFill()), contentAlignment = Alignment.Center) {
        DrawableIcon(icon, size = 20.dp, tint = Promtuz.colors.accent)
    }
}

@Composable
fun SubjectThumbStack(thumbs: List<androidx.compose.ui.graphics.ImageBitmap?>) {
    // Front to back: 32, 28 and 24 square, each peeking 12 further right.
    val shape = RoundedCornerShape(6.dp)
    Box(Modifier.size(width = 64.dp, height = 36.dp)) {
        thumbs.take(3).withIndex().reversed().forEach { (i, bmp) ->
            val side = (32 - i * 4).dp
            Box(Modifier.offset(x = (i * 12).dp, y = (2 + i * 2).dp).size(side).clip(shape)
                .background(raisedFill()).border(1.5.dp, Promtuz.colors.card, shape),
                contentAlignment = Alignment.Center) {
                if (bmp != null) androidx.compose.foundation.Image(bmp, null, Modifier.fillMaxSize(),
                    contentScale = androidx.compose.ui.layout.ContentScale.Crop)
                else DrawableIcon(R.drawable.oi_file_attachment, size = side * 0.5f, tint = Promtuz.colors.muted)
            }
        }
    }
}

@Composable
fun ForwardScreen(key: Routes.Forward, app: AppVM) {
    val targets by remember { sendTargets() }.collectAsState(emptyList())
    val selected = rememberSaveable(saver = listSaver({ it.toList() }, { it.toMutableStateList() })) { mutableStateListOf<String>() }
    var comment by rememberSaveable { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()
    SendToScreen(
        title = "Forward to…",
        targets = targets,
        selected = selected,
        onToggle = { if (!selected.remove(it)) selected.add(it); error = null },
        subject = SendToSubject(key.summary) { SubjectIconTile(R.drawable.oi_forward) },
        comment = comment,
        onComment = { comment = it },
        commentHint = "Add a comment",
        canSend = selected.isNotEmpty() && !busy,
        busy = busy,
        error = error,
    ) {
        busy = true
        scope.launch {
            try {
                val failed = CoreBridge.forwardMessages(key.conversation.fromHex(), key.dispatchIds.map { it.fromHex() },
                    selected.map { it.fromHex() }, comment).map { it.toHex() }
                if (failed.isNotEmpty()) {
                    // The rest already have it, so a retry goes only to these.
                    selected.retainAll(failed)
                    error = if (failed.size == 1) "Couldn't forward to one chat. Try again." else "Couldn't forward to ${failed.size} chats. Try again."
                    return@launch
                }
                val only = selected.singleOrNull()?.let { id -> targets.firstOrNull { it.conversation == id } }
                app.navigator.back()
                if (only != null) app.openChat(only.conversation, only.name)
            } catch (e: uniffi.core.CoreException) {
                error = (e as? uniffi.core.CoreException.Refused)?.msg ?: "Couldn't forward this"
            } finally { busy = false }
        }
    }
}
