package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.theme.Radii
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.R
import com.promtuz.chat.navigation.LocalNavCardExiting
import com.promtuz.chat.presentation.viewmodel.PickedSticker
import com.promtuz.chat.presentation.viewmodel.StickersVM
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.utils.media.EncodedImage
import org.koin.androidx.compose.koinViewModel

/** Must match the protocol limits in common::proto::sticker. */
private const val PackMax = 100
private const val NameMax = 64
private const val Columns = 4

@Composable
fun NewStickerPackScreen(
    packHex: String?,
    onDone: () -> Unit,
    viewModel: StickersVM = koinViewModel(),
) {
    val c = Promtuz.colors
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
        viewModel.pickMedia(uris, room)
    }
    val pick = { picker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageAndVideo)) }

    val nameLength = name.codePointCount(0, name.length)
    val ready = !publishing && !decoding && picks.isNotEmpty() &&
        (adding || (name.isNotBlank() && nameLength <= NameMax))

    GroupedScreen({ Text(if (adding) existing?.name ?: "Add stickers" else "New sticker pack") }) { padding ->
        Column(
            Modifier.fillMaxSize().consumeWindowInsets(padding).imePadding()
                .verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            if (!adding) {
                AppTextField(
                    name, { viewModel.draftName = it },
                    label = "Pack name",
                    placeholder = "Name your pack",
                    enabled = !publishing,
                    isError = nameLength > NameMax,
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                    suffix = { Text("$nameLength/$NameMax", style = RowSubtitleStyle, color = if (nameLength > NameMax) c.danger else c.muted) },
                    shape = RoundedCornerShape(Radii.card),
                    colors = AppTextFieldDefaults.colors().copy(label = c.accent),
                )
                CardGap()
            }

            val tiles = picks.size + if (picks.size < room && !publishing) 1 else 0
            Column(
                Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(c.card).padding(12.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                (0 until tiles).chunked(Columns).forEach { row ->
                    Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        row.forEach { i ->
                            Box(Modifier.weight(1f).aspectRatio(1f)) {
                                if (i < picks.size) PickTile(picks[i], enabled = !publishing) { picks.removeAt(i) }
                                else AddTile(decoding, pick)
                            }
                        }
                        repeat(Columns - row.size) { Spacer(Modifier.weight(1f)) }
                    }
                }
            }
            Text(
                when {
                    picks.isEmpty() -> "Images keep their shape and are shown whole, never cropped."
                    else -> "${picks.size} ${if (picks.size == 1) "sticker" else "stickers"}. Images keep their shape and are shown whole, never cropped."
                },
                Modifier.padding(start = 16.dp, end = 16.dp, top = 8.dp),
                style = TextStyle(fontSize = 13.sp, lineHeight = 18.sp), color = c.muted,
            )
            error?.let {
                Text(it, Modifier.padding(start = 16.dp, end = 16.dp, top = 8.dp), style = RowSubtitleStyle, color = c.danger)
            }
            Spacer(Modifier.height(20.dp))
            AppButton(
                when {
                    adding -> if (picks.size == 1) "Add 1 sticker" else "Add ${picks.size} stickers"
                    else -> "Create pack"
                },
                onClick = { viewModel.publish(packHex) },
                modifier = Modifier.fillMaxWidth(),
                enabled = ready || publishing,
                busy = publishing,
            )
        }
    }
}

@Composable
private fun AddTile(decoding: Boolean, onClick: () -> Unit) {
    val c = Promtuz.colors
    Column(
        Modifier.fillMaxSize().clip(RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.tile)).background(c.cardHigh).clickable(enabled = !decoding, onClick = onClick),
        verticalArrangement = Arrangement.spacedBy(4.dp, Alignment.CenterVertically),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        if (decoding) CircularProgressIndicator(Modifier.size(22.dp), color = c.accent, strokeWidth = 2.dp)
        else {
            DrawableIcon(R.drawable.oi_image, size = 24.dp, tint = c.accent)
            Text("Add", style = TextStyle(fontSize = 13.sp, lineHeight = 16.sp, fontWeight = FontWeight.Medium), color = c.accent)
        }
    }
}

@Composable
private fun PickTile(pick: PickedSticker, enabled: Boolean, onRemove: () -> Unit) {
    val c = Promtuz.colors
    val ratio = pick.preview?.let { it.width.toFloat() / it.height.coerceAtLeast(1) } ?: 1f
    // The badge sits on the image's own corner, not the cell's.
    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { Box(Modifier.aspectRatio(ratio, matchHeightConstraintsFirst = ratio < 1f)) {
        EncodedImage(
            poster = pick.preview, contentDescription = null,
            modifier = Modifier.fillMaxSize(), contentScale = ContentScale.Fit,
            maxEdge = 512,
        )
        // The hit area is larger than the badge so the corner is easy to tap.
        if (enabled) Box(
            Modifier.align(Alignment.TopEnd).offset(10.dp, (-10).dp).size(36.dp)
                .clip(CircleShape).clickable(onClickLabel = "Remove", onClick = onRemove),
            contentAlignment = Alignment.Center,
        ) {
            Box(Modifier.size(22.dp).clip(CircleShape).background(c.cardHigh), contentAlignment = Alignment.Center) {
                MorphIcon(MorphGlyph.Close, "Remove", Modifier.size(10.dp), tint = c.text, strokeWidth = 1.75.dp)
            }
        }
    } }
}
