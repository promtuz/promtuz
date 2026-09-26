package com.promtuz.chat.ui.screens

import android.text.format.Formatter
import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.promtuz.chat.R
import com.promtuz.chat.data.storage.*
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.stage.ChatMotion
import java.text.DateFormat
import java.util.Date

@Composable
fun StorageScreen(
    onOpenBackup: (() -> Unit)? = null,
    onOpenChat: ((String, String) -> Unit)? = null,
    conversation: String? = null,
    chatName: String? = null,
) {
    val context = LocalContext.current
    val source = remember(context) { StorageRepository(context) }
    StorageManager(source, onOpenBackup, onOpenChat, conversation, chatName)
}

@Composable
internal fun StorageManager(
    source: StorageSource,
    onOpenBackup: (() -> Unit)? = null,
    onOpenChat: ((String, String) -> Unit)? = null,
    chat: String? = null,
    chatName: String? = null,
) {
    val context = LocalContext.current
    val direction = LocalLayoutDirection.current
    val listState = rememberLazyListState()
    val previews = remember(source) { StoragePreviews() }
    val model = androidx.lifecycle.viewmodel.compose.viewModel { StorageVM(source) }
    val usage = model.usage
    val media = model.media
    val busy = model.busy
    val error = model.error
    val mediaError = model.mediaError
    var selected by rememberSaveable { mutableStateOf(emptySet<String>()) }
    var tab by rememberSaveable { mutableIntStateOf(if (chat == null) 0 else 1) }
    var confirmItems by remember { mutableStateOf<List<StorageMediaItem>?>(null) }
    var confirmCopies by remember { mutableStateOf(false) }
    var confirmStickers by remember { mutableStateOf(false) }
    fun size(bytes: Long) = Formatter.formatShortFileSize(context, bytes)

    fun refresh(action: (suspend () -> String)? = null) = model.refresh(action)
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { model.refresh() }
    LaunchedEffect(media) {
        if (model.usage != null && !mediaError) selected = selected.intersect(media.map { it.key }.toSet())
    }
    val scopedMedia = remember(media, chat) { media.filter { chat == null || it.conversation == chat } }
    val tabs = remember(scopedMedia, chat) {
        listOf("Chats", "All", "Photos", "Files", "Voice").withIndex().filter { (index, _) ->
            when (index) {
                0 -> chat == null && scopedMedia.isNotEmpty()
                1 -> scopedMedia.isNotEmpty()
                else -> scopedMedia.any { it.kind == index - 1 }
            }
        }
    }
    val activeTab = tab.takeIf { value -> tabs.any { it.index == value } } ?: tabs.firstOrNull()?.index ?: tab
    LaunchedEffect(tabs) { if (tabs.isNotEmpty()) tab = activeTab }
    val visible = remember(scopedMedia, activeTab) {
        scopedMedia.filter { activeTab <= 1 || it.kind == activeTab - 1 }.sortedByDescending { it.bytes }
    }
    val chosen = remember(media, selected) { media.filter { it.key in selected } }
    BackHandler(selected.isNotEmpty()) { selected = emptySet() }
    val backDispatcher = LocalOnBackPressedDispatcherOwner.current?.onBackPressedDispatcher
    val navigationMorph = rememberMorphIconState(if (selected.isNotEmpty()) MorphGlyph.Close else MorphGlyph.Back)

    SimpleScreen(
        scrollableState = listState,
        title = { Text(chatName ?: "Storage", maxLines = 1, overflow = TextOverflow.Ellipsis) },
        navigationIcon = {
            IconButton(onClick = {
                if (selected.isNotEmpty()) selected = emptySet() else backDispatcher?.onBackPressed()
            }) {
                TopBarMorphIcon(navigationMorph, if (selected.isNotEmpty()) "Close selection" else "Go Back",
                    tint = MaterialTheme.colorScheme.onSurface)
            }
        }, actions = {
            AppDropMenu(
                iconSize = 20.dp,
                anchor = { DrawableIcon(R.drawable.i_more_vert, Modifier.padding(12.dp), desc = "Storage options") },
                groups = listOf(listOf(
                    MenuAction("Refresh", R.drawable.i_refresh) { refresh() },
                )),
            )
        },
        snackbarHost = { SnackbarHost(model.snackbars) },
        floatingActionButtonPosition = FabPosition.Center,
        floatingActionButton = {
            AnimatedVisibility(chosen.isNotEmpty(), enter = fadeIn(), exit = fadeOut()) {
                Button(onClick = { confirmItems = chosen.toList() }, enabled = !busy && !mediaError,
                    modifier = Modifier.fillMaxWidth(0.9f).heightIn(min = 52.dp)) {
                    Text(if (busy) "Working…" else "Delete ${chosen.size} · ${size(chosen.sumOf { it.bytes })}")
                }
            }
        },
    ) { padding ->
        LazyColumn(state = listState, modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(start = padding.calculateStartPadding(direction) + 18.dp, end = padding.calculateEndPadding(direction) + 18.dp, top = padding.calculateTopPadding(),
                bottom = padding.calculateBottomPadding() + if (chosen.isNotEmpty()) 88.dp else 24.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp)) {
            if (busy && usage == null) item("progress") { LinearProgressIndicator(Modifier.fillMaxWidth()) }
            if (chat == null) {
                item("overview") {
                    StorageOverview(usage, media, selected, mediaError || usage == null, ::size)
                }
                if (!mediaError && media.isNotEmpty()) item("categories") {
                    Column(Modifier.fillMaxWidth().clip(RoundedCornerShape(24.dp))
                        .background(MaterialTheme.colorScheme.surfaceContainerLow)) {
                        (1..3).filter { kind -> media.any { it.kind == kind } }
                            .sortedByDescending { kind -> media.filter { it.kind == kind }.sumOf { it.bytes } }.forEach { kind ->
                            val rows = media.filter { it.kind == kind }
                            val checked = rows.all { it.key in selected }
                            Row(Modifier.fillMaxWidth().toggleable(checked, !busy, Role.Checkbox) {
                                val keys = rows.map { it.key }.toSet()
                                selected = if (checked) selected - keys else selected + keys
                            }.padding(18.dp, 13.dp), verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                                StorageCheck(checked, mediaColor(kind))
                                Text(when (kind) { 1 -> "Photos"; 2 -> "Files"; else -> "Voice messages" },
                                    Modifier.weight(1f), style = MaterialTheme.typography.bodyLarge)
                                Text(size(rows.sumOf { it.bytes }), color = MaterialTheme.colorScheme.primary,
                                    style = MaterialTheme.typography.labelLarge)
                            }
                        }
                    }
                    Spacer(Modifier.height(16.dp))
                }
            }
            item("browser") {
                Column {
                    if (tabs.isNotEmpty()) SecondaryScrollableTabRow(
                        selectedTabIndex = tabs.indexOfFirst { it.index == activeTab },
                        edgePadding = 0.dp, divider = {}, containerColor = Color.Transparent,
                    ) {
                        tabs.forEach { (index, label) ->
                            Tab(selected = activeTab == index, onClick = { tab = index; selected = emptySet() }, text = { Text(label) })
                        }
                    }
                }
            }
            if (mediaError) item("media-error") {
                Text("Couldn’t load chat media. Use Refresh in the menu to try again.", Modifier.padding(16.dp), color = MaterialTheme.colorScheme.error)
            } else if (visible.isEmpty() && !busy && chat != null) item("empty") {
                Column(Modifier.fillMaxWidth().padding(vertical = 32.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                    Text("No media stored", style = MaterialTheme.typography.titleMedium)
                }
            } else if (activeTab == 0) {
                val chats = visible.groupBy { it.conversation }.values
                    .sortedByDescending { rows -> rows.sumOf { it.bytes } }
                items(chats, key = { "chat:${it.first().conversation}" }) { rows ->
                    StorageRow(rows.first().chat, "${rows.size} media messages", size(rows.sumOf { it.bytes }),
                        enabled = onOpenChat != null && !busy,
                        onClick = {
                            selected = emptySet()
                            onOpenChat?.invoke(rows.first().conversation, rows.first().chat)
                        }) {
                        Box(Modifier.size(42.dp).clip(CircleShape).background(MaterialTheme.colorScheme.secondaryContainer), contentAlignment = Alignment.Center) {
                            Text(rows.first().chat.take(1).uppercase(), style = MaterialTheme.typography.titleMedium)
                        }
                    }
                }
            } else {
                items(visible, key = { it.key }) { item ->
                    val title = item.name.ifBlank { item.caption.ifBlank { when (item.kind) { 1 -> "Photo"; 3 -> "Voice message"; else -> "File" } } }
                    val date = remember(item.timestamp) { DateFormat.getDateInstance(DateFormat.MEDIUM).format(Date(item.timestamp * 1000)) }
                    StorageRow(title, "${item.chat} · $date", size(item.bytes), enabled = !busy,
                        checked = item.key in selected, onClick = {
                            selected = if (item.key in selected) selected - item.key else selected + item.key
                        }) {
                        Box(Modifier.size(52.dp)) {
                            StorageMediaPreview(source, previews, item)
                            Box(Modifier.align(Alignment.BottomEnd)) {
                                StorageCheck(item.key in selected, mediaColor(item.kind), 22.dp)
                            }
                        }
                    }
                }
            }
            if (chat == null) {
                val actions = buildList<Pair<String, @Composable (Int, Int) -> Unit>> {
                    if ((usage?.stickers ?: 0) > 0) add("sticker-cache" to { index, count ->
                        GroupedActionRow("Clear sticker cache", index, count, { confirmStickers = true },
                            enabled = !busy, supportingText = size(usage!!.stickers)) {
                            DrawableIcon(R.drawable.oi_sticker, size = 26.dp)
                        }
                    })
                    if ((usage?.shared ?: 0) > 0) add("copies" to { index, count ->
                        GroupedActionRow("Clear shared copies", index, count, { confirmCopies = true },
                            enabled = !busy, supportingText = size(usage!!.shared)) {
                            DrawableIcon(R.drawable.oi_trash, size = 26.dp)
                        }
                    })
                    if (onOpenBackup != null) add("backup" to { index, count ->
                        GroupedActionRow("Backup & restore", index, count, onOpenBackup) {
                            DrawableIcon(R.drawable.i_encrypted, size = 26.dp)
                        }
                    })
                }
                if (actions.isNotEmpty()) item("actions-space") { Spacer(Modifier.height(16.dp)) }
                actions.forEachIndexed { index, (key, action) -> item(key) { action(index, actions.size) } }

            }
            error?.let { message -> item("error") { Text(message, color = MaterialTheme.colorScheme.error) } }
        }
    }
    val pending = confirmItems.orEmpty()
    if (confirmItems != null) AppAlertDialog(onDismissRequest = { confirmItems = null },
        title = { Text("Delete ${pending.size} media messages?") },
        text = { Text("The selected messages, including their captions, will be deleted from this device. They cannot be downloaded again automatically. Other people’s copies won’t change.") },
        confirmButton = { TextButton(onClick = {
            val targets = pending.toList()
            confirmItems = null
            refresh {
                val removed = source.remove(targets)
                selected = emptySet()
                if (removed == targets.size) "Deleted $removed media messages"
                else "Deleted $removed of ${targets.size}. Unavailable or active items were kept."
            }
        }) { Text("Delete from this device", color = MaterialTheme.colorScheme.error) } },
        dismissButton = { TextButton(onClick = { confirmItems = null }) { Text("Cancel") } })
    if (confirmStickers) AppAlertDialog(onDismissRequest = { confirmStickers = false },
        title = { Text("Clear sticker cache?") },
        text = { Text("Messages and sticker packs will stay. Sticker images will download again when needed.") },
        confirmButton = { TextButton(onClick = { confirmStickers = false; refresh {
            source.clearStickerCache()
            "Sticker cache cleared"
        } }) { Text("Clear cache") } },
        dismissButton = { TextButton(onClick = { confirmStickers = false }) { Text("Cancel") } })
    if (confirmCopies) AppAlertDialog(onDismissRequest = { confirmCopies = false },
        title = { Text("Clear shared copies?") },
        text = { Text("Shared images and logs will be removed. Previously shared file links may stop working. Original messages and media will stay.") },
        confirmButton = { TextButton(onClick = { confirmCopies = false; refresh {
            if (source.clearSharedCopies()) "Shared copies cleared" else "Some copies couldn’t be removed. Try again."
        } }) { Text("Clear copies") } },
        dismissButton = { TextButton(onClick = { confirmCopies = false }) { Text("Cancel") } })
}

@Composable
private fun StorageRow(title: String, subtitle: String, bytes: String, enabled: Boolean = true,
    checked: Boolean? = null, onClick: () -> Unit, leading: @Composable () -> Unit) {
    val interaction = if (checked == null) Modifier.clickable(enabled = enabled, onClick = onClick)
        else Modifier.toggleable(checked, enabled, Role.Checkbox) { onClick() }
    Row(Modifier.fillMaxWidth().clip(RoundedCornerShape(18.dp)).background(MaterialTheme.colorScheme.surfaceContainerLow)
        .then(interaction).padding(14.dp), verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        leading()
        Column(Modifier.weight(1f)) {
            Text(title, maxLines = 1, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.bodyLarge)
            Text(subtitle, maxLines = 1, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Text(bytes, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.primary)
    }
}

@Composable
private fun StorageCheck(checked: Boolean, color: Color, diameter: androidx.compose.ui.unit.Dp = 24.dp) {
    val fill by animateColorAsState(if (checked) color else color.copy(alpha = 0.15f), ChatMotion.spec(), label = "selection")
    Box(Modifier.size(diameter).clip(CircleShape).background(fill), contentAlignment = Alignment.Center) {
        AnimatedVisibility(checked) {
            MorphIcon(MorphGlyph.Check, null, Modifier.size(diameter / 2), tint = Color.White, strokeWidth = 1.25.dp)
        }
    }
}

private fun mediaColor(kind: Int) = when (kind) {
    1 -> Color(0xFF5B9EEA)
    2 -> Color(0xFF56B986)
    else -> Color(0xFFE3AC54)
}

@Composable
private fun StorageOverview(usage: StorageUsage?, media: List<StorageMediaItem>, selected: Set<String>,
    unavailable: Boolean, format: (Long) -> String) {
    val grouped = remember(media) { media.groupBy { it.kind } }
    val totals = remember(grouped) { (1..3).map { kind -> grouped[kind].orEmpty().sumOf { it.bytes } } }
    val total = totals.sum()
    val colors = (1..3).map { kind ->
        val all = grouped[kind].orEmpty()
        animateColorAsState(mediaColor(kind).copy(alpha = if (selected.isEmpty() || all.any { it.key in selected }) 1f else .2f),
            ChatMotion.spec(), label = "chart selection").value
    }
    val track = MaterialTheme.colorScheme.surfaceContainerHigh
    var ready by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) { ready = true }
    val sweeps = totals.map { bytes ->
        animateFloatAsState(if (ready && !unavailable && total > 0) bytes.toFloat() / total * 360f else 0f,
            ChatMotion.spec(), label = "storage segment").value
    }
    Column(Modifier.fillMaxWidth().padding(top = 12.dp, bottom = 24.dp), horizontalAlignment = Alignment.CenterHorizontally) {
        if (!unavailable && media.isEmpty()) {
            Text("No media stored", Modifier.padding(top = 24.dp, bottom = 8.dp),
                style = MaterialTheme.typography.titleLarge)
        } else Box(Modifier.size(224.dp), contentAlignment = Alignment.Center) {
            Canvas(Modifier.fillMaxSize()) {
                val width = 42.dp.toPx()
                val origin = Offset(width / 2, width / 2)
                val arc = Size(size.width - width, size.height - width)
                drawArc(track, 0f, 360f, false, origin, arc, style = Stroke(width))
                var start = -90f
                sweeps.forEachIndexed { index, sweep ->
                    if (!unavailable && sweep > 0f) drawArc(colors[index], start,
                        (sweep - minOf(3f, sweep / 4)), false, origin, arc, style = Stroke(width))
                    start += sweep
                }
            }
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                Text(if (unavailable) "…" else format(if (selected.isEmpty()) total else media.filter { it.key in selected }.sumOf { it.bytes }),
                    style = MaterialTheme.typography.headlineLarge)
                Text(if (selected.isEmpty()) "Chat media" else "Selected", style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
        usage?.let {
            Text("${format(it.total)} app data · ${format(it.free)} free on device",
                Modifier.padding(top = 16.dp), style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant)
            LinearProgressIndicator(progress = { if (it.capacity > 0) (1f - it.free.toFloat() / it.capacity).coerceIn(0f, 1f) else 0f },
                modifier = Modifier.padding(top = 10.dp).width(200.dp).height(4.dp))
        }
    }
}
