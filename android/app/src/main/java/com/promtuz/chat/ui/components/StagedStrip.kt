package com.promtuz.chat.ui.components

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.STAGED_ATTACHMENT
import com.promtuz.chat.domain.model.StagedMedia
import com.promtuz.chat.ui.appearance.LocalChatColors

/** Tile edge; the strip's height follows from it plus the row's own padding. */
private val TileSize = 60.dp
private val TileRadius = 12.dp

/**
 * The composer's media buffer, drawn as a row of tiles above the input.
 *
 * A tile shows its preview the moment it's picked and rings a progress
 * indicator over it until the encode lands, so the wait is visible where the
 * item is rather than as a frozen send button. A failed item keeps its place
 * with an error tint — it has to be removed deliberately, since silently
 * dropping a pick reads as the app losing it.
 */
@Composable
fun StagedStrip(items: List<StagedMedia>, onRemove: (ULong) -> Unit, modifier: Modifier = Modifier) {
    LazyRow(
        modifier,
        contentPadding = PaddingValues(horizontal = 4.dp, vertical = 6.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        // toLong(): a lazy key is stashed in a Bundle for state restore, and a
        // boxed ULong isn't a type it can hold.
        items(items, key = { it.id.toLong() }) { item -> StagedTile(item) { onRemove(item.id) } }
    }
}

@Composable
private fun StagedTile(item: StagedMedia, onRemove: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current

    Box(Modifier.size(TileSize)) {
        Box(
            Modifier
                .fillMaxSize()
                .clip(RoundedCornerShape(TileRadius))
                .background(if (item.failed) colors.error.copy(alpha = 0.18f) else colors.surfaceVariant),
            contentAlignment = Alignment.Center,
        ) {
            item.preview?.let {
                Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
            }
            // A file with no visual preview: name it, since one grey square looks
            // like any other in a multi-pick.
            if (item.preview == null && item.kind == STAGED_ATTACHMENT) Text(
                item.name.takeLast(10),
                Modifier.padding(horizontal = 4.dp),
                style = MaterialTheme.typography.labelSmall,
                color = colors.onSurfaceVariant,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )

            when {
                // Scrim under the ring so it reads over a bright photo.
                !item.ready && !item.failed -> Box(
                    Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.35f)),
                    contentAlignment = Alignment.Center,
                ) {
                    CircularProgressIndicator(
                        Modifier.size(22.dp),
                        color = chat.accent,
                        strokeWidth = 2.dp,
                    )
                }
                item.failed -> DrawableIcon(
                    R.drawable.i_close,
                    Modifier.size(20.dp),
                    tint = colors.error,
                )
            }
        }

        // Sits on the corner, half outside the tile, so it never covers the
        // preview it belongs to.
        Box(
            Modifier
                .align(Alignment.TopEnd)
                .padding(2.dp)
                .size(18.dp)
                .clip(CircleShape)
                .background(colors.scrim.copy(alpha = 0.7f))
                .clickable(onClick = onRemove),
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(R.drawable.i_close, Modifier.size(11.dp), tint = Color.White)
        }
    }
}
