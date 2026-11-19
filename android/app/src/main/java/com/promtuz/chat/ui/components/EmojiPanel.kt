package com.promtuz.chat.ui.components

import android.content.Context
import androidx.compose.animation.animateColorAsState
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.LocalIndication
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.indication
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.PressInteraction
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.rememberLazyGridState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.foundation.layout.Spacer
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.graphics.FilterQuality
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ColorFilter
import androidx.compose.ui.graphics.ColorMatrix
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.onClick
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Popup
import androidx.compose.ui.window.PopupProperties
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.ui.text.EmojiGlyph
import com.promtuz.chat.ui.text.EmojiPack
import com.promtuz.chat.ui.text.EmojiSequences
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.Radii
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** One emoji and its skin-tone or gender variants, the first being the default. */
private class EmojiItem(val variants: List<String>) {
    val base get() = variants.first()
}

private class EmojiCategory(val key: String, val title: String, val cover: String, val items: List<EmojiItem>)

/** Categories and order from AndroidX's bundled lists; glyphs come from our own pack. */
private object EmojiCatalog {
    private val files = listOf(
        Triple("people", "Smileys and people", "😀") to listOf(androidx.emoji2.emojipicker.R.raw.emoji_category_emotions, androidx.emoji2.emojipicker.R.raw.emoji_category_people),
        Triple("nature", "Animals and nature", "🐻") to listOf(androidx.emoji2.emojipicker.R.raw.emoji_category_animals_nature),
        Triple("food", "Food and drink", "🍕") to listOf(androidx.emoji2.emojipicker.R.raw.emoji_category_food_drink),
        Triple("activity", "Activities", "⚽") to listOf(androidx.emoji2.emojipicker.R.raw.emoji_category_activity),
        Triple("travel", "Travel and places", "🚲") to listOf(androidx.emoji2.emojipicker.R.raw.emoji_category_travel_places),
        Triple("objects", "Objects", "💡") to listOf(androidx.emoji2.emojipicker.R.raw.emoji_category_objects),
        Triple("symbols", "Symbols", "❤️") to listOf(androidx.emoji2.emojipicker.R.raw.emoji_category_symbols),
        Triple("flags", "Flags", "🏳️") to listOf(androidx.emoji2.emojipicker.R.raw.emoji_category_flags),
    )
    @Volatile private var cached: List<EmojiCategory>? = null

    suspend fun load(context: Context, index: EmojiPack.Index): List<EmojiCategory> = cached ?: withContext(Dispatchers.IO) {
        val drawable = { e: String -> index.keys.isEmpty() || EmojiSequences.assetKey(e, index) != null }
        files.map { (meta, raws) ->
            // Emotions and people share a few emoji, and a repeated key crashes the grid when it scrolls in.
            val items = raws.flatMap { raw ->
                context.resources.openRawResource(raw).bufferedReader().useLines { lines ->
                    lines.mapNotNull { line ->
                        line.split(',').filter { it.isNotBlank() && drawable(it) }.distinct().takeIf { it.isNotEmpty() }?.let(::EmojiItem)
                    }.toList()
                }
            }.distinctBy { it.base }
            EmojiCategory(meta.first, meta.second, meta.third, items)
        }.also { if (index.keys.isNotEmpty()) cached = it }
    }
}

private const val RecentMax = 32
private val SectionLabel = TextStyle(fontSize = 13.sp, lineHeight = 18.sp, fontWeight = FontWeight.Medium)
private val Mono = ColorFilter.colorMatrix(ColorMatrix().apply { setToSaturation(0f) })

private enum class ComposerPanelTab { Emoji, Stickers }
private val PanelTabs = listOf(GlassTabItem("Emoji"), GlassTabItem("Stickers"))

// Process-wide, so reopening the panel lands where it was left.
private var lastPanelTab by mutableStateOf(ComposerPanelTab.Emoji)

/** Emoji and stickers in one panel, switched by a floating glass bar like the attach panel's. */
@Composable
fun EmojiStickerPanel(
    onEmoji: (String) -> Unit,
    onBackspace: () -> Unit,
    stickers: @Composable (bottomPadding: Dp) -> Unit,
) {
    val local = rememberHazeState()
    val barSpace = 64.dp
    Box(Modifier.fillMaxSize()) {
        Box(Modifier.fillMaxSize().hazeSource(local)) {
            when (lastPanelTab) {
                ComposerPanelTab.Emoji -> EmojiPanelBody(onEmoji, barSpace)
                ComposerPanelTab.Stickers -> stickers(barSpace)
            }
        }
        GlassTabBar(local, PanelTabs, lastPanelTab.ordinal, { lastPanelTab = ComposerPanelTab.entries[it] }, Modifier.align(Alignment.BottomCenter))
        if (lastPanelTab == ComposerPanelTab.Emoji) Box(
            Modifier.align(Alignment.BottomEnd).navigationBarsPadding().padding(end = 12.dp, bottom = 6.dp)
                .size(44.dp).floatingSurface(CircleShape, local)
                .repeatingPress(onBackspace)
                .semantics { contentDescription = "Delete"; role = Role.Button; onClick { onBackspace(); true } },
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(R.drawable.i_backspace, size = 22.dp, tint = Promtuz.colors.text)
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun EmojiPanelBody(onEmoji: (String) -> Unit, bottomPadding: Dp) {
    val context = LocalContext.current
    val c = Promtuz.colors
    EmojiPack.ensureLoaded(context)
    val index by EmojiPack.index.collectAsState()
    val categories by produceState<List<EmojiCategory>?>(null, index) {
        index?.let { value = EmojiCatalog.load(context, it) }
    }
    // Shown as of opening; picks reorder only the saved list, so cells never jump under the finger.
    val recents = remember { ChatPrefs.emojiRecent }
    var saved by remember { mutableStateOf(recents) }
    var tones by remember { mutableStateOf(ChatPrefs.emojiTones) }
    val scope = rememberCoroutineScope()
    val pick: (EmojiItem, String) -> Unit = { item, emoji ->
        onEmoji(emoji)
        // State changes here so quick taps each build on the last; only the writes leave Main, in order.
        val nextRecents = (listOf(emoji) + saved.filter { it != emoji }).take(RecentMax)
        saved = nextRecents
        val nextTones = if (item.variants.size > 1 && tones[item.base] != emoji)
            (if (emoji == item.base) tones - item.base else tones + (item.base to emoji)).also { tones = it }
            else null
        scope.launch(PrefWrites) {
            ChatPrefs.emojiRecent = nextRecents
            nextTones?.let { ChatPrefs.emojiTones = it }
        }
    }
    val sections = remember(categories, recents) {
        buildList {
            if (recents.isNotEmpty()) add(EmojiCategory("recent", "Recent", "", recents.map { EmojiItem(listOf(it)) }))
            addAll(categories.orEmpty())
        }
    }
    val starts = remember(sections) {
        var i = 0
        sections.map { s -> i.also { i += 1 + s.items.size } }
    }
    val grid = rememberLazyGridState()
    val current by remember(sections, starts) {
        derivedStateOf { sections.getOrNull(starts.indexOfLast { it <= grid.firstVisibleItemIndex })?.key }
    }

    Column(Modifier.fillMaxSize()) {
        LazyRow(
            Modifier.fillMaxWidth().height(52.dp),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            verticalAlignment = Alignment.CenterVertically,
            contentPadding = PaddingValues(horizontal = 8.dp),
        ) {
            items(sections.size, key = { sections[it].key }) { i ->
                val section = sections[i]
                val selected = section.key == current
                val fill by animateColorAsState(if (selected) c.glassFill else Color.Transparent, label = "emojiTab")
                Box(
                    Modifier.size(40.dp).clip(RoundedCornerShape(Radii.small)).background(fill)
                        .combinedClickable(onClick = { scope.launch { grid.animateScrollToItem(starts[i]) } })
                        .semantics { contentDescription = section.title },
                    contentAlignment = Alignment.Center,
                ) {
                    if (section.key == "recent") DrawableIcon(R.drawable.oi_clock, tint = if (selected) c.text else c.muted, size = 20.dp)
                    else Box(Modifier.size(22.dp).graphicsLayer { alpha = if (selected) 1f else 0.55f }) {
                        PackEmoji(section.cover, index, Modifier.fillMaxSize(), mono = !selected)
                    }
                }
            }
        }
        LazyVerticalGrid(
            GridCells.Adaptive(46.dp),
            Modifier.fillMaxSize(),
            state = grid,
            contentPadding = PaddingValues(start = 6.dp, end = 6.dp, bottom = bottomPadding),
        ) {
            sections.forEach { section ->
                item("h:${section.key}", span = { GridItemSpan(maxLineSpan) }) {
                    Text(section.title, Modifier.padding(start = 8.dp, top = 6.dp, bottom = 4.dp), style = SectionLabel, color = c.muted)
                }
                items(section.items.size, key = { "${section.key}:${section.items[it].base}" }) { i ->
                    val item = section.items[i]
                    EmojiCell(item, tones[item.base] ?: item.base, index, onPick = { pick(item, it) })
                }
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun EmojiCell(item: EmojiItem, shown: String, index: EmojiPack.Index?, onPick: (String) -> Unit) {
    val haptic = LocalHapticFeedback.current
    var choosing by remember { mutableStateOf(false) }
    Box(
        Modifier.aspectRatio(1f).clip(RoundedCornerShape(Radii.small))
            .combinedClickable(
                onClick = { onPick(shown) },
                onLongClick = if (item.variants.size > 1) ({ haptic.performHapticFeedback(HapticFeedbackType.LongPress); choosing = true }) else null,
            )
            .semantics { contentDescription = shown },
        contentAlignment = Alignment.Center,
    ) {
        PackEmoji(shown, index, Modifier.size(32.dp))
        if (choosing) VariantPicker(item, index, onDismiss = { choosing = false }) { choosing = false; onPick(it) }
    }
}

@OptIn(ExperimentalFoundationApi::class, ExperimentalLayoutApi::class)
@Composable
private fun VariantPicker(item: EmojiItem, index: EmojiPack.Index?, onDismiss: () -> Unit, onPick: (String) -> Unit) {
    val perRow = minOf(item.variants.size, 6)
    val density = androidx.compose.ui.platform.LocalDensity.current
    val lift = with(density) { (44.dp * ((item.variants.size + perRow - 1) / perRow) + 16.dp).roundToPx() }
    Popup(
        alignment = Alignment.TopCenter,
        offset = IntOffset(0, -lift),
        onDismissRequest = onDismiss,
        properties = PopupProperties(focusable = true),
    ) {
        FlowRow(
            Modifier.floatingSurface(RoundedCornerShape(Radii.menu)).padding(6.dp).requiredWidth(44.dp * perRow),
            maxItemsInEachRow = perRow,
        ) {
            item.variants.forEach { v ->
                Box(
                    Modifier.size(44.dp).clip(RoundedCornerShape(Radii.small)).combinedClickable(onClick = { onPick(v) })
                        .semantics { contentDescription = v },
                    contentAlignment = Alignment.Center,
                ) { PackEmoji(v, index, Modifier.size(30.dp)) }
            }
        }
    }
}

/** An emoji drawn from the bundled pack, falling back to the system font when the pack lacks it. */
// Drawn straight from the cached bitmap: a scrolling grid of hundreds of cells can't afford a layout per glyph.
@Composable
private fun PackEmoji(emoji: String, index: EmojiPack.Index?, modifier: Modifier, mono: Boolean = false) {
    val key = remember(emoji, index) { index?.let { EmojiSequences.assetKey(emoji, it) } }
    if (key == null) {
        Box(modifier, contentAlignment = Alignment.Center) { Text(emoji, style = TextStyle(fontSize = 24.sp)) }
        return
    }
    val context = LocalContext.current
    val bitmap = remember(key) { mutableStateOf(EmojiPack.peek(key)) }
    if (bitmap.value == null) LaunchedEffect(key) { bitmap.value = EmojiPack.glyph(context, key) }
    val filter = if (mono) Mono else null
    Spacer(modifier.drawBehind {
        bitmap.value?.let {
            drawImage(it, dstSize = IntSize(size.width.toInt(), size.height.toInt()), colorFilter = filter,
                filterQuality = FilterQuality.Medium)
        }
    })
}

private val PrefWrites = Dispatchers.IO.limitedParallelism(1)

/** Fires on press, then repeats while held, like a keyboard's delete key. */
@Composable
private fun Modifier.repeatingPress(action: () -> Unit): Modifier {
    val latest by rememberUpdatedState(action)
    val scope = rememberCoroutineScope()
    val source = remember { MutableInteractionSource() }
    return indication(source, LocalIndication.current).pointerInput(Unit) {
        awaitEachGesture {
            val down = awaitFirstDown()
            val press = PressInteraction.Press(down.position)
            source.tryEmit(press)
            val repeat = scope.launch {
                latest()
                delay(400)
                while (true) { latest(); delay(60) }
            }
            val up = waitForUpOrCancellation()
            repeat.cancel()
            source.tryEmit(if (up != null) PressInteraction.Release(press) else PressInteraction.Cancel(press))
        }
    }
}
