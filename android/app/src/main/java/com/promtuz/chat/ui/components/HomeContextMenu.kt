package com.promtuz.chat.ui.components

import dev.chrisbanes.haze.HazeState
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.size
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.layer.drawLayer
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.unit.dp
import kotlin.math.roundToInt

/** [pressRoot] is where the finger went down, in root coordinates. */
/** [row] and [snapshot] lift the pressed row above the blurred backdrop so it's clear which chat the menu acts on. */
data class HomeMenuAnchor(
    val pressRoot: Offset,
    val groups: List<List<MenuAction>>,
    val row: androidx.compose.ui.geometry.Rect? = null,
    val snapshot: androidx.compose.ui.graphics.layer.GraphicsLayer? = null,
)

class HomeMenuState : DragMenuState<HomeMenuAnchor>(12)

/** [HomeMenuState.close] plays the exit, then releases the anchor. */
@Composable
fun HomeContextMenu(state: HomeMenuState, haze: HazeState) {
    val anchor = state.anchor ?: return

    var origin by remember { mutableStateOf(Offset.Zero) }
    Box(Modifier.fillMaxSize().onGloballyPositioned { origin = it.positionInRoot() }) {
        MenuBackdrop(state, haze, dim = 0.28f, scrimMs = 300, popMs = 220, exitMs = 150)
        val row = anchor.row
        val snapshot = anchor.snapshot
        if (row != null && snapshot != null) {
            val density = androidx.compose.ui.platform.LocalDensity.current
            val page = com.promtuz.chat.ui.theme.Promtuz.colors.card
            Box(
                Modifier
                    .offset { androidx.compose.ui.unit.IntOffset((row.left - origin.x).roundToInt(), (row.top - origin.y).roundToInt()) }
                    .size(with(density) { row.width.toDp() }, with(density) { row.height.toDp() })
                    .graphicsLayer {
                        val p = state.scrim.value / 0.28f
                        alpha = p.coerceIn(0f, 1f)
                    }
                    .clip(androidx.compose.foundation.shape.RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.card))
                    .background(page)
                    .drawBehind { drawLayer(snapshot) },
            )
        }

        // The card sits at the finger, clamped inside the bars and IME so it never sinks behind them.
        Box(
            Modifier
                .fillMaxSize()
                .windowInsetsPadding(WindowInsets.systemBars.union(WindowInsets.ime)),
        ) {
            var stackOrigin by remember { mutableStateOf(Offset.Zero) }
            Box(Modifier.fillMaxSize().onGloballyPositioned { stackOrigin = it.positionInRoot() }) {
                Layout(
                    content = {
                        MenuCard(
                            iconSize = 20.dp,
                            groups = anchor.groups,
                            hovered = state.hovered,
                            modifier = Modifier.graphicsLayer {
                                val p = state.pop.value
                                alpha = p.coerceIn(0f, 1f)
                                scaleX = 0.8f + 0.2f * p
                                scaleY = 0.8f + 0.2f * p
                                transformOrigin = TransformOrigin(0f, 0f)
                            },
                            itemHeight = 46.dp,
                            haze = haze,
                            onRowPositioned = { i, c -> state.targets[i] = c },
                            onPick = { it.onClick(); state.close() },
                        )
                    },
                    modifier = Modifier.fillMaxSize(),
                ) { measurables, constraints ->
                    val loose = constraints.copy(minWidth = 0, minHeight = 0)
                    val card = measurables[0].measure(loose)
                    layout(constraints.maxWidth, constraints.maxHeight) {
                        val margin = 8.dp.roundToPx()
                        val px = (anchor.pressRoot.x - stackOrigin.x).roundToInt()
                        // Under the lifted row, or above it when the row sits too low.
                        val below = row?.let { (it.bottom - stackOrigin.y).roundToInt() + margin }
                        val above = row?.let { (it.top - stackOrigin.y).roundToInt() - margin - card.height }
                        val py = when {
                            below == null -> (anchor.pressRoot.y - stackOrigin.y).roundToInt()
                            below + card.height <= constraints.maxHeight - margin -> below
                            else -> above!!
                        }
                        val x = px.coerceIn(margin, (constraints.maxWidth - card.width - margin).coerceAtLeast(margin))
                        val y = py.coerceIn(margin, (constraints.maxHeight - card.height - margin).coerceAtLeast(margin))
                        card.place(x, y)
                    }
                }
            }
        }
    }
}
