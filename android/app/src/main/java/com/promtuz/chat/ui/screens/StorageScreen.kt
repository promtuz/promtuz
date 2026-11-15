package com.promtuz.chat.ui.screens

import android.text.format.Formatter
import androidx.activity.compose.BackHandler
import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.lazy.grid.rememberLazyGridState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.FabPosition
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.scale
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.promtuz.chat.R
import com.promtuz.chat.data.storage.*
import com.promtuz.chat.domain.model.mediaLabel
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.stage.ChatMotion
import com.promtuz.chat.ui.theme.Promtuz
import kotlinx.coroutines.CancellationException

@Composable
fun StorageScreen(
    onOpenChat: ((String, String) -> Unit)? = null,
    conversation: String? = null,
    chatName: String? = null,
) {
    val context = LocalContext.current
    val source = remember(context) { StorageRepository(context) }
    StorageManager(source, onOpenChat, conversation, chatName)
}

private enum class Order(val label: String) { Largest("Largest"), Newest("Newest"), Oldest("Oldest") }

@Composable
internal fun StorageManager(
    source: StorageSource,
    onOpenChat: ((String, String) -> Unit)? = null,
    chat: String? = null,
    chatName: String? = null,
) {
    val context = LocalContext.current
    val model = androidx.lifecycle.viewmodel.compose.viewModel { StorageVM(source) }
    val usage = model.usage
    val busy = model.busy
    var selected by rememberSaveable { mutableStateOf(emptySet<String>()) }
    var confirmItems by remember { mutableStateOf<List<StorageMediaItem>?>(null) }
    var confirmCopies by remember { mutableStateOf(false) }
    var confirmStickers by remember { mutableStateOf(false) }
    fun size(bytes: Long) = Formatter.formatShortFileSize(context, bytes)

    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { model.refresh() }
    LaunchedEffect(model.media) {
        if (usage != null && !model.mediaError) selected = selected.intersect(model.media.map { it.key }.toSet())
    }
    val chosen = remember(model.media, selected) { model.media.filter { it.key in selected } }
    BackHandler(selected.isNotEmpty()) { selected = emptySet() }
    val backDispatcher = LocalOnBackPressedDispatcherOwner.current?.onBackPressedDispatcher
    val navigationMorph = rememberMorphIconState(if (selected.isNotEmpty()) MorphGlyph.Close else MorphGlyph.Back)
    val gridState = rememberLazyGridState()

    GroupedScreen(
        scrollableState = if (chat != null) gridState else null,
        title = {
            Text(if (selected.isNotEmpty()) "${selected.size} selected" else chatName ?: "Storage",
                maxLines = 1, overflow = TextOverflow.Ellipsis)
        },
        navigationIcon = {
            IconButton(onClick = { if (selected.isNotEmpty()) selected = emptySet() else backDispatcher?.onBackPressed() }) {
                TopBarMorphIcon(navigationMorph, if (selected.isNotEmpty()) "Close selection" else "Go Back", tint = Promtuz.colors.text)
            }
        },
        actions = {
            AppDropMenu(
                iconSize = 20.dp,
                anchor = { DrawableIcon(R.drawable.i_more_vert, Modifier.padding(12.dp), desc = "Storage options") },
                groups = listOf(listOf(MenuAction("Refresh", R.drawable.i_refresh) { model.refresh() })),
            )
        },
        snackbarHost = { SnackbarHost(model.snackbars) },
        floatingActionButtonPosition = FabPosition.Center,
        floatingActionButton = {
            AnimatedVisibility(chosen.isNotEmpty(), enter = fadeIn(), exit = fadeOut()) {
                AppButton(
                    if (busy) "Working…" else "Delete ${chosen.size} · ${size(chosen.sumOf { it.bytes })}",
                    onClick = { confirmItems = chosen.toList() }, enabled = !busy && !model.mediaError,
                    modifier = Modifier.fillMaxWidth(0.9f),
                )
            }
        },
    ) { padding ->
        if (chat == null) Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState())
                .padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            Overview(model, ::size, onOpenChat, onClearStickers = { confirmStickers = true }, onClearCopies = { confirmCopies = true })
        } else ChatMedia(
            source, model, chat, gridState, padding, selected, ::size,
            onToggle = { key -> selected = if (key in selected) selected - key else selected + key },
            bottom = if (chosen.isNotEmpty()) 88.dp else 24.dp,
        )
    }

    val pending = confirmItems.orEmpty()
    if (confirmItems != null) AppAlertDialog(onDismissRequest = { confirmItems = null },
        title = { Text(if (pending.size == 1) "Delete this item?" else "Delete ${pending.size} items?") },
        text = { Text("They’ll be deleted from this phone with their messages and captions. They can’t be downloaded again. Other people keep their copies.") },
        confirmButton = { TextButton(onClick = {
            val targets = pending.toList()
            confirmItems = null
            model.refresh {
                val removed = source.remove(targets)
                selected = emptySet()
                if (removed == targets.size) "Deleted $removed" else "Deleted $removed of ${targets.size}. Items in use were kept."
            }
        }) { Text("Delete", color = Promtuz.colors.danger) } },
        dismissButton = { TextButton(onClick = { confirmItems = null }) { Text("Cancel") } })
    if (confirmStickers) AppAlertDialog(onDismissRequest = { confirmStickers = false },
        title = { Text("Clear sticker cache?") },
        text = { Text("Messages and sticker packs stay. Sticker images download again when needed.") },
        confirmButton = { TextButton(onClick = { confirmStickers = false; model.refresh {
            source.clearStickerCache()
            "Sticker cache cleared"
        } }) { Text("Clear") } },
        dismissButton = { TextButton(onClick = { confirmStickers = false }) { Text("Cancel") } })
    if (confirmCopies) AppAlertDialog(onDismissRequest = { confirmCopies = false },
        title = { Text("Clear shared copies?") },
        text = { Text("Files you shared out of Promtuz will be removed, so links to them may stop working. Your messages and media stay.") },
        confirmButton = { TextButton(onClick = { confirmCopies = false; model.refresh {
            if (source.clearSharedCopies()) "Shared copies cleared" else "Some copies couldn’t be removed. Try again."
        } }) { Text("Clear") } },
        dismissButton = { TextButton(onClick = { confirmCopies = false }) { Text("Cancel") } })
}

private data class Slice(val label: String, val bytes: Long, val color: Color)

private fun kindColor(kind: Int) = when (kind) {
    1 -> Color(0xFF2E9BF0)
    2 -> Color(0xFFD9A441)
    else -> Color(0xFF58B98A)
}

@Composable
private fun Overview(
    model: StorageVM,
    size: (Long) -> String,
    onOpenChat: ((String, String) -> Unit)?,
    onClearStickers: () -> Unit,
    onClearCopies: () -> Unit,
) {
    val c = Promtuz.colors
    val usage = model.usage
    val media = model.media
    val slices = remember(usage, media) {
        if (usage == null) emptyList() else {
            val byKind = (1..3).associateWith { kind -> media.filter { it.kind == kind }.sumOf { it.bytes } }
            val messages = usage.entries.firstOrNull { it.category == StorageCategory.Chats }?.bytes ?: 0
            val known = byKind.values.sum() + usage.stickers + messages
            listOf(
                Slice("Photos", byKind.getValue(1), kindColor(1)),
                Slice("Files", byKind.getValue(2), kindColor(2)),
                Slice("Voice", byKind.getValue(3), kindColor(3)),
                Slice("Stickers", usage.stickers, Color(0xFFE0735F)),
                Slice("Messages", messages, Color(0xFFB287D9)),
                Slice("Other", (usage.total - known).coerceAtLeast(0), c.muted),
            ).filter { it.bytes > 0 }
        }
    }
    TotalCard(usage?.total, slices, size)

    if (model.mediaError) {
        CardGap()
        SettingsCard(header = "Chats") {
            SettingsRow("Couldn’t load chat media", subtitle = "Tap to try again", onClick = { model.refresh() })
        }
    } else if (media.isNotEmpty()) {
        CardGap()
        val chats = remember(media) {
            media.groupBy { it.conversation }.values.sortedByDescending { rows -> rows.sumOf { it.bytes } }
        }
        var all by rememberSaveable { mutableStateOf(false) }
        SettingsCard(Modifier.animateContentSize(ChatMotion.spec()), header = "Chats") {
            (if (all) chats else chats.take(3)).forEach { rows ->
                val first = rows.first()
                SettingsRow(
                    first.chat,
                    subtitle = size(rows.sumOf { it.bytes }),
                    leading = { ChatAvatar(first, 40.dp) },
                    leadingSize = 40.dp,
                    trailing = Trailing.Chevron,
                    onClick = onOpenChat?.let { { it(first.conversation, first.chat) } },
                )
            }
            if (!all && chats.size > 3) SettingsRow(
                "Show all ${chats.size} chats",
                titleColor = c.accent,
                leading = { DrawableIcon(R.drawable.i_users, size = 24.dp, tint = c.accent) },
                leadingSize = 40.dp,
                onClick = { all = true },
            )
        }
    }

    val stickers = usage?.stickers ?: 0
    val shared = usage?.shared ?: 0
    if (stickers > 0 || shared > 0) {
        CardGap()
        SettingsCard(header = "Copies you can clear") {
            if (stickers > 0) SettingsRow("Sticker cache", subtitle = "Downloaded again when needed",
                value = size(stickers), trailing = Trailing.Chevron, enabled = !model.busy, onClick = onClearStickers)
            if (shared > 0) SettingsRow("Shared copies", subtitle = "Files you shared out of Promtuz",
                value = size(shared), trailing = Trailing.Chevron, enabled = !model.busy, onClick = onClearCopies)
        }
    }
    model.error?.let {
        CardGap()
        Text(it, Modifier.padding(horizontal = 16.dp), style = RowSubtitleStyle, color = c.danger)
    }
    Text(
        "Your messages and attachments are never cleared automatically." +
            if (media.isNotEmpty()) " Delete them from a chat above." else "",
        Modifier.padding(start = 16.dp, end = 16.dp, top = 12.dp),
        style = TextStyle(fontSize = 13.sp, lineHeight = 18.sp), color = c.muted,
    )
}

@Composable
private fun TotalCard(total: Long?, slices: List<Slice>, size: (Long) -> String) {
    val c = Promtuz.colors
    var ready by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) { ready = true }
    val sum = slices.sumOf { it.bytes }.coerceAtLeast(1)
    Column(
        Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(c.card).padding(16.dp),
    ) {
        Text(total?.let(size) ?: "…", style = TextStyle(fontSize = 32.sp, lineHeight = 38.sp, fontWeight = FontWeight.Medium), color = c.text)
        Text("Used by Promtuz on this phone", style = RowSubtitleStyle, color = c.muted)
        if (slices.isNotEmpty()) {
            Row(Modifier.padding(top = 16.dp).fillMaxWidth().height(10.dp), horizontalArrangement = Arrangement.spacedBy(3.dp)) {
                slices.forEach { slice ->
                    // Tiny slices keep a sliver, so every legend entry has a mark on the bar.
                    val share by animateFloatAsState(if (ready) maxOf(slice.bytes.toFloat() / sum, 0.015f) else 0.0001f,
                        ChatMotion.spec(), label = "slice")
                    Box(Modifier.weight(share).fillMaxHeight().clip(RoundedCornerShape(3.dp)).background(slice.color))
                }
            }
            Column(Modifier.padding(top = 14.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                slices.chunked(2).forEach { pair ->
                    Row(horizontalArrangement = Arrangement.spacedBy(24.dp)) {
                        pair.forEach { slice ->
                            Row(Modifier.weight(1f), verticalAlignment = Alignment.CenterVertically) {
                                Box(Modifier.size(8.dp).clip(CircleShape).background(slice.color))
                                Text(slice.label, Modifier.padding(start = 8.dp).weight(1f), style = RowValueStyle, color = c.text)
                                Text(size(slice.bytes), style = TextStyle(fontSize = 13.sp, lineHeight = 18.sp), color = c.muted)
                            }
                        }
                        if (pair.size == 1) Spacer(Modifier.weight(1f))
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ChatMedia(
    source: StorageSource,
    model: StorageVM,
    chat: String,
    state: androidx.compose.foundation.lazy.grid.LazyGridState,
    padding: PaddingValues,
    selected: Set<String>,
    size: (Long) -> String,
    onToggle: (String) -> Unit,
    bottom: androidx.compose.ui.unit.Dp,
) {
    val c = Promtuz.colors
    val previews = remember(source) { StoragePreviews() }
    var order by rememberSaveable { mutableStateOf(Order.Largest) }
    val items = remember(model.media, chat, order) {
        model.media.filter { it.conversation == chat }.let { rows ->
            when (order) {
                Order.Largest -> rows.sortedByDescending { it.bytes }
                Order.Newest -> rows.sortedByDescending { it.timestamp }
                Order.Oldest -> rows.sortedBy { it.timestamp }
            }
        }
    }
    LazyVerticalGrid(
        GridCells.Fixed(3), Modifier.fillMaxSize(), state,
        contentPadding = padding.listPadding(top = 6.dp, bottom = bottom),
        horizontalArrangement = Arrangement.spacedBy(3.dp),
        verticalArrangement = Arrangement.spacedBy(3.dp),
    ) {
        item("head", span = { GridItemSpan(maxLineSpan) }) {
            val first = items.firstOrNull()
            Row(
                Modifier.padding(bottom = 9.dp).fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(c.card).padding(16.dp),
                horizontalArrangement = Arrangement.spacedBy(14.dp), verticalAlignment = Alignment.CenterVertically,
            ) {
                first?.let { ChatAvatar(it, 48.dp) }
                Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    Text(size(items.sumOf { it.bytes }), style = TextStyle(fontSize = 22.sp, lineHeight = 28.sp, fontWeight = FontWeight.Medium), color = c.text)
                    Text(counts(items).ifEmpty { "Nothing stored" }, style = RowSubtitleStyle, color = c.muted)
                }
            }
        }
        if (items.size > 1) item("order", span = { GridItemSpan(maxLineSpan) }) {
            SegmentedTabs(Order.entries.map { it.label }, order.ordinal, Modifier.padding(bottom = 9.dp)) { order = Order.entries[it] }
        }
        if (model.mediaError) item("error", span = { GridItemSpan(maxLineSpan) }) {
            Text("Couldn’t load this chat’s media. Use Refresh in the menu to try again.",
                Modifier.padding(16.dp), style = RowSubtitleStyle, color = c.danger)
        }
        items(items, key = { it.key }) { item ->
            MediaTile(source, previews, item, item.key in selected, selected.isNotEmpty(), size, enabled = !model.busy) {
                onToggle(item.key)
            }
        }
        if (items.isNotEmpty()) item("foot", span = { GridItemSpan(maxLineSpan) }) {
            Text("Tap to pick what to delete. Deleted media is gone from this phone for good.",
                Modifier.padding(start = 16.dp, end = 16.dp, top = 9.dp),
                style = TextStyle(fontSize = 13.sp, lineHeight = 18.sp), color = c.muted)
        }
    }
}

@Composable
private fun ChatAvatar(item: StorageMediaItem, size: androidx.compose.ui.unit.Dp) = Avatar(
    item.chat, size = size, identityKey = item.avatar,
    image = com.promtuz.chat.utils.media.rememberAvatar(item.avatar.takeUnless { it.startsWith("group:") || it == item.conversation }),
)

private fun counts(items: List<StorageMediaItem>): String {
    fun n(count: Int, one: String, many: String) = if (count == 1) "1 $one" else "$count $many"
    return listOfNotNull(
        items.count { it.kind == 1 }.takeIf { it > 0 }?.let { n(it, "photo", "photos") },
        items.count { it.kind == 2 }.takeIf { it > 0 }?.let { n(it, "file", "files") },
        items.count { it.kind == 3 }.takeIf { it > 0 }?.let { n(it, "voice message", "voice messages") },
    ).joinToString(", ")
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun MediaTile(
    source: StorageSource,
    previews: StoragePreviews,
    item: StorageMediaItem,
    checked: Boolean,
    selecting: Boolean,
    size: (Long) -> String,
    enabled: Boolean,
    onToggle: () -> Unit,
) {
    val c = Promtuz.colors
    val bitmap by produceState<ImageBitmap?>(null, source, item.key) {
        if (item.kind == 1) try { value = previews.get(source, item) }
        catch (e: CancellationException) { throw e }
        catch (_: Exception) { /* The tile still shows its kind and size. */ }
    }
    val inset by animateFloatAsState(if (checked) 0.88f else 1f, ChatMotion.spec(), label = "tile")
    Box(
        Modifier.aspectRatio(1f).clip(RoundedCornerShape(4.dp)).background(if (checked) c.card else Color.Transparent)
            .combinedClickable(enabled = enabled, role = Role.Checkbox, onLongClick = onToggle, onClick = onToggle),
    ) {
        Box(Modifier.fillMaxSize().scale(inset).clip(RoundedCornerShape(if (checked) 10.dp else 4.dp)).background(c.cardHigh)) {
            bitmap?.let { Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop) } ?: Column(
                Modifier.fillMaxSize().padding(10.dp),
                horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center,
            ) {
                DrawableIcon(when (item.kind) { 1 -> R.drawable.oi_image; 3 -> R.drawable.i_mic; else -> R.drawable.oi_paperclip },
                    size = 24.dp, tint = c.muted)
                if (item.kind != 1) Text(item.name.ifBlank { mediaLabel(item.kind) }, Modifier.padding(top = 6.dp),
                    style = TextStyle(fontSize = 12.sp, lineHeight = 15.sp), color = c.text, maxLines = 2,
                    overflow = TextOverflow.Ellipsis, textAlign = TextAlign.Center)
            }
            Text(
                size(item.bytes),
                Modifier.align(Alignment.BottomStart).padding(6.dp).clip(RoundedCornerShape(6.dp))
                    .background(Color.Black.copy(alpha = 0.55f)).padding(horizontal = 6.dp, vertical = 2.dp),
                style = TextStyle(fontSize = 11.sp, lineHeight = 14.sp, fontWeight = FontWeight.Medium), color = Color.White,
            )
        }
        AnimatedVisibility(selecting, Modifier.align(Alignment.TopEnd).padding(6.dp), enter = scaleIn() + fadeIn(), exit = scaleOut() + fadeOut()) {
            val fill by animateColorAsState(if (checked) c.accentFill else Color.Black.copy(alpha = 0.25f), ChatMotion.spec(), label = "badge")
            Box(
                Modifier.size(22.dp).clip(CircleShape).background(fill).border(1.5.dp, Color.White, CircleShape),
                contentAlignment = Alignment.Center,
            ) {
                AnimatedVisibility(checked, enter = scaleIn() + fadeIn(), exit = scaleOut() + fadeOut()) {
                    MorphIcon(MorphGlyph.Check, null, Modifier.size(12.dp), tint = Color.White, strokeWidth = 1.5.dp)
                }
            }
        }
    }
}
