package com.promtuz.chat.ui.components

import android.content.Context
import android.view.accessibility.AccessibilityManager
import androidx.annotation.DrawableRes
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.DpOffset
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Popup
import androidx.compose.ui.window.PopupProperties

data class MenuAction(
    val label: String,
    @param:DrawableRes val icon: Int? = null,
    val destructive: Boolean = false,
    val onClick: () -> Unit,
)

/**
 * Our dropdown, so height/shape/spacing are ours instead of M3's forced 48dp / 8dp internals.
 * Owns the anchor because press-drag-select needs one continuous pointer stream: the down that
 * opens the menu is the same one that drags to an item.
 *
 * Tap the anchor -> menu opens and stays for normal tapping.
 * Press-and-drag -> hover items, release over one to pick it, release anywhere else to cancel.
 */
@Composable
fun AppDropMenu(
    anchor: @Composable () -> Unit,
    groups: List<List<MenuAction>>,
    modifier: Modifier = Modifier,
    itemHeight: Dp = 48.dp,
    verticalPadding: Dp = 0.dp,
    dragSelect: Boolean = true,
    offset: DpOffset = DpOffset(0.dp, 0.dp),
    shape: Shape = RoundedCornerShape(20.dp),
) {
    val density = LocalDensity.current
    val context = LocalContext.current
    val touchExploration = remember {
        (context.getSystemService(Context.ACCESSIBILITY_SERVICE) as? AccessibilityManager)
            ?.isTouchExplorationEnabled == true
    }
    val haptic = LocalHapticFeedback.current
    val flat = remember(groups) { groups.flatten() }

    var expanded by remember { mutableStateOf(false) }
    var dragging by remember { mutableStateOf(false) }
    var tapHeld by remember { mutableStateOf(false) } // opened by a tap -> allow outside-tap dismiss
    var hovered by remember { mutableIntStateOf(-1) }
    var menuWidthPx by remember { mutableIntStateOf(0) }
    val vis = remember { MutableTransitionState(false) }
    vis.targetState = expanded

    fun close() { expanded = false; tapHeld = false; dragging = false; hovered = -1 }

    Box(modifier) {
        Box(
            Modifier
                .pointerInput(groups, dragSelect, touchExploration) {
                    val itemPx = itemHeight.toPx()
                    val vpadPx = verticalPadding.toPx()
                    val offX = offset.x.toPx()
                    val offY = offset.y.toPx()

                    // The menu is a separate Popup window, so its items live in another coordinate
                    // space we can't hit-test against. Instead the menu sits at a known spot
                    // (covering the anchor, right-aligned) and we map the finger by math.
                    // ponytail: press starts over item 0 since the menu covers the icon; the
                    // touch-slop drag gate guards accidental commits. Ignores divider thickness
                    // (~1dp each); fine at 48dp rows.
                    fun indexAt(px: Float, py: Float): Int {
                        if (menuWidthPx == 0) return -1
                        val top = offY
                        val right = size.width + offX
                        if (py < top + vpadPx) return -1
                        if (px < right - menuWidthPx || px > right) return -1
                        val i = ((py - top - vpadPx) / itemPx).toInt()
                        return if (i in flat.indices) i else -1
                    }

                    awaitEachGesture {
                        val down = awaitFirstDown(requireUnconsumed = false)
                        if (!vis.isIdle) { down.consume(); return@awaitEachGesture } // ignore spam mid-animation
                        if (expanded) { close(); down.consume(); return@awaitEachGesture }
                        expanded = true; dragging = false; hovered = -1
                        if (!dragSelect || touchExploration) { tapHeld = true; return@awaitEachGesture }

                        while (true) {
                            val change = awaitPointerEvent().changes.firstOrNull() ?: break
                            if (!change.pressed) {
                                if (dragging) {
                                    if (hovered in flat.indices) {
                                        haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                                        flat[hovered].onClick()
                                    }
                                    close()
                                } else tapHeld = true
                                hovered = -1
                                break
                            }
                            if (!dragging &&
                                (change.position - down.position).getDistance() > viewConfiguration.touchSlop
                            ) dragging = true
                            if (dragging) {
                                val i = indexAt(change.position.x, change.position.y)
                                if (i != hovered) {
                                    hovered = i
                                    if (i >= 0) haptic.performHapticFeedback(HapticFeedbackType.SegmentTick)
                                }
                                change.consume()
                            }
                        }
                    }
                }
        ) { anchor() }

        if (vis.currentState || vis.targetState) {
            val offXpx = with(density) { offset.x.roundToPx() }
            val offYpx = with(density) { offset.y.roundToPx() }
            Popup(
                alignment = Alignment.TopEnd,
                offset = IntOffset(offXpx, offYpx),
                onDismissRequest = { close() },
                // Non-focusable while dragging: a focusable window opening mid-gesture can cancel the
                // in-flight drag. Once the menu is just held open by a tap, focusable enables
                // outside-tap dismiss.
                properties = PopupProperties(focusable = tapHeld && !dragging),
            ) {
                AnimatedVisibility(
                    visibleState = vis,
                    enter = scaleIn(tween(120), 0.85f, TransformOrigin(1f, 0f)) + fadeIn(tween(120)),
                    exit = scaleOut(tween(100), 0.85f, TransformOrigin(1f, 0f)) + fadeOut(tween(100)),
                ) {
                    Surface(
                        shape = shape,
                        color = MaterialTheme.colorScheme.surfaceContainer,
                        tonalElevation = 3.dp,
                        shadowElevation = 8.dp,
                        modifier = Modifier.onSizeChanged { menuWidthPx = it.width },
                    ) {
                        Column(Modifier.width(IntrinsicSize.Max).padding(vertical = verticalPadding)) {
                            var i = 0
                            groups.forEachIndexed { gi, group ->
                                if (gi != 0) HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
                                group.forEach { action ->
                                    MenuRow(action, itemHeight, hovered == i) { action.onClick(); close() }
                                    i++
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun MenuRow(action: MenuAction, itemHeight: Dp, highlighted: Boolean, onClick: () -> Unit) {
    val color =
        if (action.destructive) MaterialTheme.colorScheme.error
        else MaterialTheme.colorScheme.onSurface
    Row(
        Modifier
            .fillMaxWidth()
            .widthIn(min = 160.dp, max = 280.dp)
            .height(itemHeight)
            .background(
                if (highlighted) MaterialTheme.colorScheme.surfaceContainerHighest else Color.Transparent
            )
            .clickable(role = Role.Button, onClickLabel = action.label, onClick = onClick)
            .padding(horizontal = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        action.icon?.let { DrawableIcon(it, tint = color) }
        Text(action.label, color = color, style = MaterialTheme.typography.labelLarge)
    }
}
