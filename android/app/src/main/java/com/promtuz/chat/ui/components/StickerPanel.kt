package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Radii
import androidx.compose.runtime.getValue
import androidx.compose.animation.animateColorAsState
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
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.derivedStateOf
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
import com.promtuz.chat.ui.theme.Promtuz
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.stage.ChatMotion
import com.promtuz.chat.utils.media.StickerImage
import kotlinx.coroutines.launch

internal val StickerGap = 8.dp
private val SectionLabel = TextStyle(fontSize = 13.sp, lineHeight = 18.sp, fontWeight = FontWeight.Medium)

private class Section(val key: String, val title: String, val stickers: List<StickerRef>, val start: Int)

@Composable
fun StickerPanelBody(
    packs: List<UiStickerPack>,
    recents: List<StickerRef>,
    onPick: (StickerRef) -> Unit,
    onCreate: () -> Unit,
    onManage: () -> Unit,
    bottomPadding: androidx.compose.ui.unit.Dp = 8.dp,
) {
    if (packs.isEmpty()) {
        Column(
            Modifier.fillMaxSize(),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            Text("No stickers yet", style = RowTitleStyle, color = Promtuz.colors.muted)
            AppButton("Create a pack", onCreate, Modifier.padding(top = 16.dp), kind = ButtonKind.Tonal)
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
    val c = Promtuz.colors

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().height(56.dp).padding(start = 8.dp, end = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            LazyRow(
                Modifier.weight(1f),
                horizontalArrangement = Arrangement.spacedBy(4.dp),
                contentPadding = PaddingValues(horizontal = 4.dp),
            ) {
                items(sections, key = { it.key }) { section ->
                    val selected = section.key == current
                    PanelTab(selected, section.title, { scope.launch { grid.animateScrollToItem(section.start) } }) {
                        if (section.key == "recent") DrawableIcon(R.drawable.oi_clock, tint = if (selected) c.text else c.muted, size = 20.dp)
                        else section.stickers.firstOrNull()?.let { cover ->
                            StickerImage(cover, null, Modifier.size(34.dp).clip(RoundedCornerShape(8.dp)))
                        }
                    }
                }
            }
            Box(
                Modifier.size(40.dp).clip(CircleShape).clickable(onClick = onManage)
                    .semantics { contentDescription = "Manage stickers" },
                contentAlignment = Alignment.Center,
            ) {
                DrawableIcon(R.drawable.oi_settings, tint = c.muted)
            }
        }

        LazyVerticalGrid(
            columns = GridCells.Fixed(columns),
            state = grid,
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(start = StickerGap, end = StickerGap, bottom = bottomPadding),
            horizontalArrangement = Arrangement.spacedBy(StickerGap),
            verticalArrangement = Arrangement.spacedBy(StickerGap),
        ) {
            sections.forEach { section ->
                item(key = "h:${section.key}", span = { GridItemSpan(maxLineSpan) }) {
                    Text(
                        section.title,
                        Modifier.padding(start = 6.dp, end = 6.dp, top = 4.dp, bottom = 2.dp),
                        style = SectionLabel,
                        color = c.muted,
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
    Box(
        Modifier.aspectRatio(1f).then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier),
        contentAlignment = Alignment.Center,
    ) {
        StickerImage(
            ref, null, Modifier.fillMaxSize(), contentScale = ContentScale.Fit,
            placeholderColor = Promtuz.colors.text.copy(alpha = 0.05f),
        )
    }
}

/** One tile in the emoji or sticker panel's section strip. */
@Composable
internal fun PanelTab(selected: Boolean, title: String, onClick: () -> Unit, content: @Composable () -> Unit) {
    val fill by animateColorAsState(if (selected) Promtuz.colors.glassFill else Color.Transparent, ChatMotion.spec(), label = "panel tab")
    Box(
        Modifier.size(40.dp).clip(RoundedCornerShape(Radii.small)).background(fill)
            .clickable(onClick = onClick).semantics { contentDescription = title },
        contentAlignment = Alignment.Center,
    ) { content() }
}
