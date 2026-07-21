package com.promtuz.chat.ui.components

import androidx.compose.animation.core.Animatable
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
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.appearance.LocalChatColors
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeoutOrNull
import kotlin.math.roundToInt

/**
 * The attach region: an IME-height panel that SWAPS with the software keyboard.
 * Last child of the composer [Column]; it owns the whole bottom region (why
 * [ChatBottomBar] drops `.imePadding()`/`.navigationBarsPadding()`).
 *
 * The one rule that keeps the swap jump-free: the region height is sourced from
 * ONE animating thing at a time, never a blend of two. [presence] is snapped
 * (not tweened) whenever the keyboard is the thing moving, so we ride the
 * system's own keyboard animation instead of racing it:
 *  - open with keyboard up   → snap present, hide the keyboard; its slide-down
 *    uncovers the already-full-height panel while `panelH` holds the region.
 *  - close to keyboard        → hold present while the rising keyboard covers the
 *    panel, then drop it in one step (never tween down INTO a rising inset).
 *  - open/close with no keyboard → the only case we animate ourselves (slide).
 */
@Composable
fun AttachPanel(
    open: Boolean,
    closingToKeyboard: Boolean,
    onPickPhotos: () -> Unit,
    onPickFiles: () -> Unit,
) {
    val density = LocalDensity.current
    val ime = WindowInsets.ime.getBottom(density)          // raw, system-animated
    val nav = WindowInsets.navigationBars.getBottom(density)
    val kbdUp = with(density) { 120.dp.roundToPx() }        // ime above this = keyboard really up

    // ponytail: learn the keyboard's pixel height (largest ime seen while up); a
    // single collector, not a per-frame write. Falls back to 300.dp until first seen.
    var learned by remember { mutableIntStateOf(0) }
    val imeState = rememberUpdatedState(ime)
    LaunchedEffect(Unit) {
        snapshotFlow { imeState.value }.collect { if (it > kbdUp && it > learned) learned = it }
    }
    val panelH = if (learned > 0) learned else with(density) { 300.dp.roundToPx() }

    // Panel presence 0..1. Snapped for keyboard-driven transitions, tweened only
    // when no keyboard moves — so the region never averages two curves.
    val presence = remember { Animatable(0f) }
    LaunchedEffect(open) {
        if (open) {
            if (imeState.value > kbdUp) presence.snapTo(1f)   // ride the keyboard's exit
            else presence.animateTo(1f, tween(240))            // no keyboard: slide up
        } else if (closingToKeyboard) {
            // Hold until the rising keyboard has covered the panel, then drop in one
            // step. Timeout guards a keyboard that never actually shows.
            withTimeoutOrNull(600) {
                snapshotFlow { imeState.value }.first { it >= (panelH * 0.9f).roundToInt() }
            }
            presence.snapTo(0f)
        } else {
            presence.animateTo(0f, tween(240))                 // no keyboard: slide down
        }
    }

    val regionPx = maxOf(ime, (panelH * presence.value).roundToInt(), nav)
    Box(Modifier.fillMaxWidth().height(with(density) { regionPx.toDp() })) {
        if (presence.value > 0f) {
            // Anchored bottom at the full learned height; the region uncovers it.
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
