package com.promtuz.chat.ui.screens

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.R
import com.promtuz.chat.navigation.LocalNavCardExiting
import com.promtuz.chat.presentation.viewmodel.PickedSticker
import com.promtuz.chat.presentation.viewmodel.StickersVM
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.GroupActionButton
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.MorphIcon
import com.promtuz.chat.ui.components.SimpleScreen
import org.koin.androidx.compose.koinViewModel

/** Must match the protocol limits in common::proto::sticker. */
private const val PackMax = 100
private const val NameMax = 64

/** Create a pack or append images to a pack owned by this identity. */
@Composable
fun NewStickerPackScreen(
    packHex: String?,
    onDone: () -> Unit,
    viewModel: StickersVM = koinViewModel(),
) {
    val colors = MaterialTheme.colorScheme
    val packs by viewModel.packs.collectAsStateWithLifecycle()
    val existing = packs.firstOrNull { it.packHex == packHex }
    val adding = packHex != null
    val room = PackMax - (existing?.stickers?.size ?: 0)

    val name = viewModel.draftName
    val picks = viewModel.picks
    val decoding = viewModel.decoding
    val publishing = viewModel.publishing
    val error = viewModel.error
    val exiting = LocalNavCardExiting.current
    LaunchedEffect(viewModel.published, exiting) {
        if (viewModel.published && !exiting) { viewModel.consumePublished(); onDone() }
    }

    val picker = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia()) { uris ->
        viewModel.pickImages(uris, room)
    }
    val pick = { picker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly)) }

    val nameLength = name.codePointCount(0, name.length)
    val ready = !publishing && !decoding && picks.isNotEmpty() &&
        (adding || (name.isNotBlank() && nameLength <= NameMax))

    SimpleScreen({ Text(if (adding) "Add stickers" else "New sticker pack") }) { padding ->
        Column(Modifier.fillMaxSize().padding(top = padding.calculateTopPadding())) {
            if (!adding) BasicTextField(
                value = name,
                onValueChange = { viewModel.draftName = it },
                enabled = !publishing,
                singleLine = true,
                textStyle = MaterialTheme.typography.titleMedium.copy(color = colors.onSurface),
                cursorBrush = SolidColor(colors.primary),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                modifier = Modifier.fillMaxWidth().padding(horizontal = 18.dp, vertical = 8.dp)
                    .clip(RoundedCornerShape(24.dp)).background(colors.surfaceContainerHigh)
                    .padding(horizontal = 18.dp, vertical = 18.dp)
                    .semantics { contentDescription = "Pack name" },
                decorationBox = { inner ->
                    Box {
                        if (name.isEmpty()) Text(
                            "Pack name",
                            color = colors.onSurfaceVariant,
                            style = MaterialTheme.typography.titleMedium,
                        )
                        inner()
                    }
                },
            ) else existing?.let {
                Text(
                    it.name, Modifier.padding(horizontal = 18.dp, vertical = 8.dp),
                    style = MaterialTheme.typography.titleMedium,
                )
            }

            if (!adding && nameLength > NameMax) Text(
                "$nameLength/$NameMax", Modifier.align(Alignment.End).padding(horizontal = 24.dp),
                color = colors.error, style = MaterialTheme.typography.labelSmall,
            )

            LazyVerticalGrid(
                columns = GridCells.Adaptive(96.dp),
                modifier = Modifier.weight(1f).fillMaxWidth(),
                contentPadding = PaddingValues(horizontal = 14.dp, vertical = 6.dp),
            ) {
                items(picks.size, key = { picks[it].uri.toString() }) { i ->
                    PickTile(picks[i], enabled = !publishing) { picks.removeAt(i) }
                }
                if (picks.size < room && !publishing) item(key = "add") {
                    Box(
                        Modifier
                            .aspectRatio(1f)
                            .padding(4.dp)
                            .clip(RoundedCornerShape(14.dp))
                            .background(colors.surfaceContainerHigh)
                            .clickable(enabled = !decoding, onClick = pick),
                        contentAlignment = Alignment.Center,
                    ) {
                        if (decoding) CircularProgressIndicator(Modifier.size(22.dp), strokeWidth = 2.dp)
                        else MorphIcon(MorphGlyph.Plus, "Add images", Modifier.size(28.dp), tint = LocalChatColors.current.accent)
                    }
                }
            }

            error?.let {
                Text(it, Modifier.padding(horizontal = 24.dp, vertical = 4.dp), color = colors.error,
                    style = MaterialTheme.typography.bodyMedium)
            }
            GroupActionButton(
                when {
                    publishing -> "Publishing…"
                    adding -> if (picks.size == 1) "Add 1 sticker" else "Add ${picks.size} stickers"
                    else -> "Create pack"
                },
                onClick = { viewModel.publish(packHex) },
                enabled = ready,
                modifier = Modifier.fillMaxWidth().padding(16.dp),
            )
        }
    }
}

@Composable
private fun PickTile(pick: PickedSticker, enabled: Boolean, onRemove: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    Box(Modifier.aspectRatio(1f).padding(4.dp)) {
        Box(
            Modifier.fillMaxSize().clip(RoundedCornerShape(14.dp)).background(colors.surfaceContainerHigh),
            contentAlignment = Alignment.Center,
        ) {
            pick.preview?.let { Image(it, null, Modifier.fillMaxSize().padding(6.dp), contentScale = ContentScale.Fit) }
        }
        if (enabled) Box(
            Modifier
                .align(Alignment.TopEnd)
                .padding(2.dp)
                .size(32.dp)
                .clip(CircleShape)
                .background(colors.scrim.copy(alpha = 0.7f))
                .clickable(onClick = onRemove),
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(R.drawable.i_close, Modifier.size(12.dp), tint = Color.White)
        }
    }
}
