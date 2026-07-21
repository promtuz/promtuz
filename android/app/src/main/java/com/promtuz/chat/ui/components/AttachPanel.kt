package com.promtuz.chat.ui.components

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.appearance.LocalChatColors
import kotlin.math.roundToInt

/**
 * The attach region: an IME-height panel that SWAPS with the software keyboard.
 * It's the last child of the composer [Column] and owns the whole bottom region —
 * the reason [ChatBottomBar] drops `.imePadding()`/`.navigationBarsPadding()`.
 *
 * The activity is `adjustResize` + edge-to-edge, so `WindowInsets.ime` is live.
 * Open with the keyboard up: the region equals the ime height and the panel is
 * drawn full-height *behind* the keyboard window; hiding the keyboard slides it
 * down and uncovers the panel with no jump. Open with the keyboard down: [anim]
 * drives the region 0→panelH and the panel slides up.
 */
@Composable
fun AttachPanel(
    open: Boolean,
    onPickPhotos: () -> Unit,
    onPickFiles: () -> Unit,
) {
    val density = LocalDensity.current
    val ime = WindowInsets.ime.getBottom(density)          // raw, system-animated
    val nav = WindowInsets.navigationBars.getBottom(density)

    // ponytail: learn the keyboard's pixel height by keeping the largest ime we've
    // seen; before that first sighting fall back to 300.dp so the panel still opens.
    var learned by remember { mutableIntStateOf(0) }
    if (ime > learned) learned = ime
    val panelH = if (learned > 0) learned else with(density) { 300.dp.roundToPx() }

    val anim by animateFloatAsState(if (open) 1f else 0f, tween(240), label = "attachPanel")
    // Region tracks the tallest of: live keyboard, the opening panel, the nav bar.
    // The nav term keeps a bottom gap under the composer when everything is closed.
    val regionPx = maxOf(ime, (panelH * anim).roundToInt(), nav)

    Box(Modifier.fillMaxWidth().height(with(density) { regionPx.toDp() })) {
        if (anim > 0f) {
            // ponytail: reveal-by-clip — the panel is always anchored to the bottom at
            // the FULL learned height and just gets uncovered as the region grows (or
            // the keyboard slides away). Opaque, no fade; the clip is the animation.
            Box(
                Modifier
                    .fillMaxWidth()
                    .height(with(density) { panelH.toDp() })
                    .align(Alignment.BottomCenter)
                    .background(MaterialTheme.colorScheme.surfaceContainer),
            ) {
                AttachPanelBody(onPickPhotos, onPickFiles)
            }
        }
    }
}

@Composable
private fun AttachPanelBody(onPickPhotos: () -> Unit, onPickFiles: () -> Unit) {
    var tab by remember { mutableStateOf(0) } // 0 = Photos, 1 = Files

    Box(Modifier.fillMaxSize()) {
        // Shell placeholder content. Each tab centers one launcher button.
        // ponytail: the inline MediaStore grid replaces the Photos placeholder later.
        Box(Modifier.fillMaxSize().navigationBarsPadding(), contentAlignment = Alignment.Center) {
            if (tab == 0) PlaceholderAction("Open gallery", onPickPhotos)
            else PlaceholderAction("Browse files", onPickFiles)
        }

        // Floating pill tabs, bottom-centered above the nav bar, over the content.
        Row(
            Modifier
                .align(Alignment.BottomCenter)
                .navigationBarsPadding()
                .padding(bottom = 12.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            PillTab("Photos", tab == 0) { tab = 0 }
            PillTab("Files", tab == 1) { tab = 1 }
        }
    }
}

@Composable
private fun PillTab(label: String, selected: Boolean, onClick: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val accent = LocalChatColors.current.accent
    val shape = RoundedCornerShape(percent = 50)
    Box(
        Modifier
            .shadow(4.dp, shape)
            .clip(shape)
            .background(if (selected) accent else colors.surfaceVariant)
            .clickable(onClick = onClick)
            .padding(horizontal = 20.dp, vertical = 10.dp),
    ) {
        Text(
            label,
            style = MaterialTheme.typography.labelLarge,
            color = if (selected) colors.onPrimary else colors.onSurfaceVariant,
        )
    }
}

@Composable
private fun PlaceholderAction(label: String, onClick: () -> Unit) {
    val accent = LocalChatColors.current.accent
    Box(
        Modifier
            .clip(RoundedCornerShape(16.dp))
            .clickable(onClick = onClick)
            .padding(horizontal = 28.dp, vertical = 16.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, style = MaterialTheme.typography.titleMedium, color = accent)
    }
}
