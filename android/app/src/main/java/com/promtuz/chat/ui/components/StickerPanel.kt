package com.promtuz.chat.ui.components

import androidx.compose.animation.animateColorAsState
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.rememberLazyGridState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.StickerRef
import com.promtuz.chat.domain.model.UiStickerPack
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.stage.ChatMotion
import com.promtuz.chat.utils.media.rememberStickerBitmap
import kotlinx.coroutines.launch

internal val StickerGap = 8.dp
private val TabSize = 36.dp

private class Section(val key: String, val title: String, val stickers: List<StickerRef>, val start: Int)

@Composable
fun StickerPanelBody(
    packs: List<UiStickerPack>,
    recents: List<StickerRef>,
    onPick: (StickerRef) -> Unit,
    onCreate: () -> Unit,
    onManage: () -> Unit,
) {
    if (packs.isEmpty()) {
        Column(
            Modifier.fillMaxSize(),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            Text(
                "No stickers yet",
                style = MaterialTheme.typography.bodyLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            PlaceholderAction("Create a pack", onCreate)
        }
        return
    }

    val sections = remember(packs, recents) {
        var index = 0
        buildList {
            if (recents.isNotEmpty()) {
                add(Section("recent", "Recent", recents, index))
                index += 1 + recents.size
            }
            packs.forEach {
                add(Section(it.packHex, it.name, it.stickers, index))
                index += 1 + it.stickers.size
            }
        }
    }
    val columns = remember { ChatPrefs.stickerColumns }
    val grid = rememberLazyGridState()
    val scope = rememberCoroutineScope()
    val current by remember(sections) {
        derivedStateOf {
            val first = grid.firstVisibleItemIndex
            sections.lastOrNull { it.start <= first }?.key ?: sections.first().key
        }
    }
    val colors = MaterialTheme.colorScheme
    val accent = LocalChatColors.current.accent

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().height(52.dp).padding(horizontal = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            LazyRow(
                Modifier.weight(1f),
                horizontalArrangement = Arrangement.spacedBy(4.dp),
                contentPadding = PaddingValues(horizontal = 4.dp),
            ) {
                items(sections, key = { it.key }) { section ->
                    val selected = section.key == current
                    val background by animateColorAsState(
                        if (selected) accent.copy(alpha = 0.22f) else Color.Transparent,
                        ChatMotion.spec(),
                        label = "pack selection",
                    )
                    Box(
                        Modifier
                            .size(TabSize + 8.dp)
                            .clip(RoundedCornerShape(10.dp))
                            .background(background)
                            .clickable { scope.launch { grid.animateScrollToItem(section.start) } }
                            .semantics { contentDescription = section.title },
                        contentAlignment = Alignment.Center,
                    ) {
                        if (section.key == "recent") Text(
                            "Recent",
                            style = MaterialTheme.typography.labelSmall,
                            color = if (selected) accent else colors.onSurfaceVariant,
                            maxLines = 1,
                        ) else section.stickers.firstOrNull()?.let { cover ->
                            rememberStickerBitmap(cover)?.let {
                                Image(it, null, Modifier.size(TabSize), contentScale = ContentScale.Fit)
                            }
                        }
                    }
                }
            }
            Box(
                Modifier.size(40.dp).clip(CircleShape).clickable(onClick = onManage)
                    .semantics { contentDescription = "Manage stickers" },
                contentAlignment = Alignment.Center,
            ) {
                DrawableIcon(R.drawable.oi_settings, tint = colors.onSurfaceVariant)
            }
        }

        LazyVerticalGrid(
            columns = GridCells.Fixed(columns),
            state = grid,
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(start = StickerGap, end = StickerGap, bottom = 8.dp),
            horizontalArrangement = Arrangement.spacedBy(StickerGap),
            verticalArrangement = Arrangement.spacedBy(StickerGap),
        ) {
            sections.forEach { section ->
                item(key = "h:${section.key}", span = { GridItemSpan(maxLineSpan) }) {
                    Text(
                        section.title,
                        Modifier.padding(start = 2.dp, top = 6.dp),
                        style = MaterialTheme.typography.labelMedium,
                        color = colors.onSurfaceVariant,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                items(section.stickers.size, key = { "${section.key}:${section.stickers[it].key}" }) { i ->
                    val ref = section.stickers[i]
                    StickerCell(ref) { onPick(ref) }
                }
            }
        }
    }
}

@Composable
fun StickerCell(ref: StickerRef, onClick: (() -> Unit)? = null) {
    val bitmap = rememberStickerBitmap(ref)
    Box(
        Modifier.aspectRatio(1f).then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier),
        contentAlignment = Alignment.Center,
    ) {
        if (bitmap != null) Image(bitmap, null, Modifier.fillMaxSize(), contentScale = ContentScale.Fit)
        else Box(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.onSurface.copy(alpha = 0.05f)))
    }
}
