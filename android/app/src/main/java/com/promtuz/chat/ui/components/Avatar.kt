package com.promtuz.chat.ui.components

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.ripple
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.media.mediaOrigin

const val AVATAR_RADIUS_RATIO = 2.875f;

/**
 * A person's tile: their picture when we hold one ([image], see
 * [com.promtuz.chat.utils.media.rememberAvatar]), their initials otherwise.
 * Tappable only when given an [onClick]; a tile with nothing to do lets the
 * tap fall through to the row it sits in.
 */
@ExperimentalMaterial3Api
@Composable
fun Avatar(
    name: String,
    size: Dp = 52.dp,
    clipRatio: Float = AVATAR_RADIUS_RATIO,
    statusColor: Color? = null,
    image: ImageBitmap? = null,
    onClick: (() -> Unit)? = null,
    originKey: String? = null,
    identityKey: String = originKey ?: name,
) {
    val clip = RoundedCornerShape(size / clipRatio)
    val fallbackChars = name.split(" ")
        .filter { it.isNotBlank() }
        .map { it.first() }
        .take(2)
        .joinToString("").uppercase()
    val interactionSource = remember { MutableInteractionSource() }

    Box(Modifier.size(size).then(if (originKey != null) Modifier.mediaOrigin(originKey, size / clipRatio) else Modifier)) {
        Box(
            Modifier
                .fillMaxSize()
                .clip(clip)
                .background(avatarColor(identityKey))
                .clickable(
                    enabled = onClick != null,
                    interactionSource = interactionSource,
                ) {
                    onClick?.invoke()
                }, contentAlignment = Alignment.Center
        ) {
            if (image != null) Image(
                image, null,
                Modifier.fillMaxSize(),
                contentScale = ContentScale.Crop,
            ) else Text(
                fallbackChars,
                fontWeight = FontWeight.Bold,
                fontSize = (size.value / 2.6f).sp,
                color = Color.White
            )
        }

        // Corner status dot; the surface-colored ring reads as a cutout over the
        // list/header behind the avatar's rounded corner.
        if (statusColor != null) {
            val dot = size * 0.28f
            Box(
                Modifier
                    .align(Alignment.BottomEnd)
                    .size(dot)
                    .clip(CircleShape)
                    .background(MaterialTheme.colorScheme.surface)
                    .padding(dot * 0.2f)
                    .clip(CircleShape)
                    .background(statusColor)
            )
        }
    }
}
/**
 * A group's avatar.
 *
 * [Avatar] derives initials from one name, which a group doesn't have — so a
 * titled group uses its title's initials, and an untitled one falls back to a
 * split of its first two members. That fallback matters: a group created
 * without a name should still look like *those people*, not like a blank tile.
 */
@ExperimentalMaterial3Api
@Composable
fun GroupAvatar(
    title: String,
    members: List<String>,
    size: Dp = 52.dp,
    clipRatio: Float = AVATAR_RADIUS_RATIO,
    conversation: String? = null,
    onClick: (() -> Unit)? = null,
) {
    val image = com.promtuz.chat.utils.media.rememberAvatar(conversation?.let { "group:$it" })
    if (image != null) {
        Avatar(title, size, clipRatio, image = image, onClick = onClick,
            originKey = conversation?.let { "group:$it" }, identityKey = conversation ?: title)
        return
    }
    if (title.isNotBlank()) {
        Avatar(name = title, size = size, clipRatio = clipRatio, identityKey = conversation ?: title, onClick = onClick)
        return
    }

    val clip = RoundedCornerShape(size / clipRatio)
    val pair = members.take(2)
    Box(
        Modifier
            .size(size)
            .clip(clip)
            .background(avatarColor(conversation ?: title)).clickable(enabled = onClick != null) { onClick?.invoke() },
    ) {
        if (pair.isEmpty()) {
            Text(
                "G",
                Modifier.align(Alignment.Center),
                fontWeight = FontWeight.Bold,
                fontSize = (size.value / 2.6f).sp,
                color = Color.White,
            )
        } else {
            // Two initials on a diagonal — legible at list size, where a 2x2
            // grid of four would just be four smudges.
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

/** Stable across renames and restores; the key never leaves this device. */
fun avatarColor(identityKey: String): Color {
    val palette = longArrayOf(0xFFB74459, 0xFF9A5527, 0xFF387A51, 0xFF237A80,
        0xFF376FB0, 0xFF7355AC, 0xFFA24882, 0xFF546A83)
    return Color(palette[Math.floorMod(identityKey.hashCode(), palette.size)])
}
