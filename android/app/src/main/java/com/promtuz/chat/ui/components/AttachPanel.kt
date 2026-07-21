package com.promtuz.chat.ui.components

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.imeAnimationTarget
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
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.chatBarHaze
import com.promtuz.chat.ui.util.freezeOnExit
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeoutOrNull
import kotlin.math.roundToInt

/**
 * The attach region: an IME-height sheet that SWAPS with the software keyboard.
 * Last child of the composer [Column]; it owns the whole bottom region (why
 * [ChatBottomBar] drops `.imePadding()`/`.navigationBarsPadding()`).
 *
 * Jump-free rule: the region height is sourced from ONE animating thing at a
 * time, never a blend. [presence] is SNAPPED (not tweened) whenever the keyboard
 * is what's moving, so we ride the system's own keyboard animation:
 *  - open with keyboard up   → snap present, THEN hide the keyboard (order matters:
 *    `panelH` must hold the region before the ime starts dropping, or it collapses
 *    for a frame). The keyboard's slide-down uncovers the already-full-height sheet.
 *  - close to keyboard        → hold present while the rising keyboard covers the
 *    sheet, then drop it in one step (never tween down INTO a rising inset).
 *  - open/close with no keyboard → the only case we animate ourselves.
 * [panelH] is learned from `imeAnimationTarget` (the SETTLED target), not the live
 * max — the live max captures the keyboard's overshoot and makes the sheet too tall.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun AttachPanel(
    open: Boolean,
    closingToKeyboard: Boolean,
    haze: HazeState,
    onHideKeyboard: () -> Unit,
    onPickPhotos: () -> Unit,
    onPickFiles: () -> Unit,
) {
    val density = LocalDensity.current
    val ime = WindowInsets.ime.getBottom(density)                      // live, system-animated
    val imeTarget = WindowInsets.imeAnimationTarget.getBottom(density) // settled target (no overshoot)
    val nav = WindowInsets.navigationBars.getBottom(density)
    val kbdUp = with(density) { 120.dp.roundToPx() }                   // ime above this = keyboard really up

    // Learn the SETTLED keyboard height. Falls back to 300.dp until first seen.
    var learned by remember { mutableIntStateOf(0) }
    val imeTargetState = rememberUpdatedState(imeTarget)
    LaunchedEffect(Unit) {
        snapshotFlow { imeTargetState.value }.collect { if (it > kbdUp) learned = it }
    }
    val panelH = if (learned > 0) learned else with(density) { 300.dp.roundToPx() }

    // Presence 0..1. Snapped for keyboard-driven transitions, tweened only when no
    // keyboard moves — so the region never averages two curves.
    val presence = remember { Animatable(0f) }
    val imeLive = rememberUpdatedState(ime)
    val hide = rememberUpdatedState(onHideKeyboard)
    LaunchedEffect(open) {
        if (open) {
            if (imeLive.value > kbdUp) {
                presence.snapTo(1f)   // present FIRST so panelH holds the region,
                hide.value()          // THEN hide the keyboard — its exit uncovers the sheet
            } else {
                presence.animateTo(1f, tween(240))  // no keyboard: slide up
            }
        } else if (closingToKeyboard) {
            // Hold the sheet until the keyboard is FULLY up (live ime has reached its
            // target), THEN drop in one step. Snapping any earlier leaves the region
            // below where the keyboard now is — that's the end-of-close jump. Timeout
            // guards a keyboard that never actually shows.
            withTimeoutOrNull(600) {
                snapshotFlow { imeLive.value to imeTargetState.value }
                    .first { (live, target) -> target > kbdUp && live >= target }
            }
            presence.snapTo(0f)
        } else {
            presence.animateTo(0f, tween(240))       // no keyboard: slide down
        }
    }

    val regionPx = maxOf(ime, (panelH * presence.value).roundToInt(), nav)
    Box(Modifier.fillMaxWidth().height(with(density) { regionPx.toDp() })) {
        if (presence.value > 0f) {
            // Anchored bottom at the full learned height; the region uncovers it. A
            // rounded-top translucent sheet sharing the composer's blur recipe.
            Box(
                Modifier
                    .align(Alignment.BottomCenter)
                    .fillMaxWidth()
                    .height(with(density) { panelH.toDp() })
                    .clip(RoundedCornerShape(topStart = 18.dp, topEnd = 18.dp))
                    .freezeOnExit()
                    .hazeEffect(haze, chatBarHaze()),
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
            .clip(shape)
            .background(if (selected) accent else colors.surfaceVariant.copy(alpha = 0.7f))
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
