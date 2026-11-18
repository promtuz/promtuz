package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Promtuz

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.media.MediaFrame
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.media.mediaOrigin
import com.promtuz.chat.utils.media.AvatarPicture
import com.promtuz.chat.utils.media.EncodedImageContent

const val AVATAR_RADIUS_RATIO = 2.875f

private val EmbeddedAvatarLayers = AvatarLayers(
    base = true,
    specular = true,
    rim = false,
    innerShadow = false,
    dropShadow = false,
    glyphLighting = false,
)

@Composable
fun Avatar(
    name: String,
    size: Dp = 52.dp,
    clipRatio: Float = AVATAR_RADIUS_RATIO,
    statusColor: Color? = null,
    image: AvatarPicture? = null,
    onClick: (() -> Unit)? = null,
    originKey: String? = null,
    identityKey: String = originKey ?: name,
) {
    val clip = if (clipRatio == AVATAR_RADIUS_RATIO) SquircleShape else RoundedCornerShape(size / clipRatio)
    val fallbackChars = remember(name) {
        name.split(" ").filter { it.isNotBlank() }.take(2)
            .joinToString("") { it.first().toString() }.uppercase()
    }
    val glyph = remember(fallbackChars) {
        if (fallbackChars.isEmpty()) AvatarGlyph.None
        else AvatarGlyph.Letter(fallbackChars, sizeFraction = 1f / 2.6f)
    }
    val baseColor = remember(identityKey) { avatarColor(identityKey) }
    val interactionSource = remember { MutableInteractionSource() }
    val frame = remember(identityKey, image) { MediaFrame() }

    Box(Modifier.size(size).then(if (originKey != null) {
        Modifier.mediaOrigin(originKey, size / clipRatio) { frame.image ?: image?.poster }
    } else Modifier)) {
        Box(
            Modifier
                .fillMaxSize()
                .clip(clip)
                .clickable(
                    enabled = onClick != null,
                    interactionSource = interactionSource,
                ) {
                    onClick?.invoke()
                }, contentAlignment = Alignment.Center
        ) {
            if (image != null) EncodedImageContent(
                prepared = image.prepared,
                contentDescription = null,
                modifier = Modifier.fillMaxSize(),
                contentScale = ContentScale.Crop,
                animate = MediaViewer.session == null,
                onFrameChanged = { frame.image = it },
            ) else SquircleAvatar(
                glyph = glyph,
                modifier = Modifier.semantics { contentDescription = name },
                size = size,
                baseColor = baseColor,
                layers = EmbeddedAvatarLayers,
                fillBounds = true,
            )
        }

        // The surface-colored ring reads as a cutout over whatever is behind the avatar.
        if (statusColor != null) {
            val dot = size * 0.28f
            Box(
                Modifier
                    .align(Alignment.BottomEnd)
                    .size(dot)
                    .clip(CircleShape)
                    .background(Promtuz.colors.page)
                    .padding(dot * 0.2f)
                    .clip(CircleShape)
                    .background(statusColor)
            )
        }
    }
}

@Composable
fun GroupAvatar(
    title: String,
    members: List<String>,
    size: Dp = 52.dp,
    clipRatio: Float = AVATAR_RADIUS_RATIO,
    conversation: String? = null,
    onClick: (() -> Unit)? = null,
    onPhotoClick: ((AvatarPicture) -> Unit)? = null,
) {
    val image = com.promtuz.chat.utils.media.rememberAvatar(conversation?.let { "group:$it" })
    if (image != null) {
        Avatar(title, size, clipRatio, image = image, onClick = onPhotoClick?.let { { it(image) } } ?: onClick,
            originKey = conversation?.let { "group:$it" }, identityKey = conversation ?: title)
        return
    }
    if (title.isNotBlank()) {
        Avatar(name = title, size = size, clipRatio = clipRatio, identityKey = conversation ?: title, onClick = onClick)
        return
    }

    val clip = if (clipRatio == AVATAR_RADIUS_RATIO) SquircleShape else RoundedCornerShape(size / clipRatio)
    val pair = members.take(2)
    val baseColor = remember(conversation, title) { avatarColor(conversation ?: title) }
    Box(
        Modifier
            .size(size)
            .clip(clip)
            .clickable(enabled = onClick != null) { onClick?.invoke() },
    ) {
        SquircleAvatar(AvatarGlyph.None, size = size, baseColor = baseColor,
            layers = EmbeddedAvatarLayers, fillBounds = true)
        if (pair.isEmpty()) {
            Text(
                "G",
                Modifier.align(Alignment.Center),
                fontWeight = FontWeight.Bold,
                fontSize = (size.value / 2.6f).sp,
                color = Color.White,
            )
        } else {
            pair.forEachIndexed { i, name ->
                Text(
                    name.trim().take(1).uppercase(),
                    Modifier
                        .align(if (i == 0) Alignment.TopStart else Alignment.BottomEnd)
                        .padding(size * 0.16f),
                    fontWeight = FontWeight.Bold,
                    fontSize = (size.value / 3.6f).sp,
                    color = Color.White.copy(if (i == 0) 1f else 0.8f),
                )
            }
        }
    }
}

/** Keyed on identity, so the color survives renames and restores. */
fun avatarColor(identityKey: String): Color = avatarPalette(identityKey.hashCode())

/** Hidden chat tags index the same palette. */
fun avatarPalette(index: Int): Color = Color(AvatarPalette[Math.floorMod(index, AvatarPalette.size)])

private val AvatarPalette = longArrayOf(0xFFB74459, 0xFF9A5527, 0xFF387A51, 0xFF237A80,
    0xFF376FB0, 0xFF7355AC, 0xFFA24882, 0xFF546A83)
