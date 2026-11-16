package com.promtuz.chat.ui.components

import android.content.Context
import android.view.animation.OvershootInterpolator
import android.view.accessibility.AccessibilityManager
import androidx.annotation.DrawableRes
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.EaseOutQuint
import androidx.compose.animation.core.Easing
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.Indication
import androidx.compose.foundation.LocalIndication
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.awaitLongPressOrCancellation
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.PressInteraction
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.size
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
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.hapticfeedback.HapticFeedback
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.AwaitPointerEventScope
import androidx.compose.ui.input.pointer.PointerInputChange
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.layout.positionOnScreen
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.DpOffset
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.toSize
import androidx.compose.ui.window.Popup
import androidx.compose.ui.window.PopupProperties
import kotlinx.coroutines.launch
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.Radii
import dev.chrisbanes.haze.HazeState
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

data class MenuAction(
    val label: String,
    @param:DrawableRes val icon: Int? = null,
    val destructive: Boolean = false,
    val iconPlaceholder: String? = null,
    val glyph: MorphGlyph? = null,
    val onClick: () -> Unit,
)

/** Explicit, since a vector drawable otherwise draws at whatever size its XML declares. */
val MenuIconSize = 24.dp

/**
 * Owns the anchor so the press that opens the menu can drag to an item and release to pick it.
 * With [onClick], a short tap performs that action and a hold opens the menu.
 */
@Composable
fun AppDropMenu(
    anchor: @Composable () -> Unit,
    groups: List<List<MenuAction>>,
    modifier: Modifier = Modifier,
    itemHeight: Dp = 48.dp,
    verticalPadding: Dp = 0.dp,
    offset: DpOffset = DpOffset(0.dp, 0.dp),
    shape: Shape = RoundedCornerShape(Radii.menu),
    iconSize: Dp = MenuIconSize,
    onClick: (() -> Unit)? = null,
    onClickLabel: String? = null,
    onLongClickLabel: String? = null,
    indication: Indication? = LocalIndication.current,
) {
    val density = LocalDensity.current
    val context = LocalContext.current
    val touchExploration = remember {
        (context.getSystemService(Context.ACCESSIBILITY_SERVICE) as? AccessibilityManager)
            ?.isTouchExplorationEnabled == true
    }
    val haptic = LocalHapticFeedback.current
    val flat by rememberUpdatedState(groups.flatten())
    val click by rememberUpdatedState(onClick)
    val pressSource = remember { MutableInteractionSource() }

    var expanded by remember { mutableStateOf(false) }
    var tapHeld by remember { mutableStateOf(false) } // opened by a tap -> allow outside-tap dismiss
    var hovered by remember { mutableIntStateOf(-1) }
    val rows = remember { mutableMapOf<Int, Rect>() }
    var anchorCoords by remember { mutableStateOf<LayoutCoordinates?>(null) }
    val vis = remember { MutableTransitionState(false) }
    vis.targetState = expanded

    fun close() { expanded = false; tapHeld = false; hovered = -1; rows.clear() }

    Box(modifier) {
        Box(
            Modifier
                .onGloballyPositioned { anchorCoords = it }
                .pointerInput(onClick != null, touchExploration) {
                    // The Popup is a separate window, so rows are hit-tested in screen space.
                    fun indexAt(p: Offset): Int {
                        val at = (anchorCoords?.takeIf { it.isAttached } ?: return -1).positionOnScreen() + p
                        return rows.entries.firstOrNull { it.value.contains(at) }?.key ?: -1
                    }

                    awaitEachGesture {
                        val longPressOnly = click != null
                        val down = awaitFirstDown(
                            requireUnconsumed = false,
                            pass = if (longPressOnly) PointerEventPass.Initial else PointerEventPass.Main,
                        )
                        if (!vis.isIdle) { down.consume(); return@awaitEachGesture } // ignore spam mid-animation
                        if (expanded) { close(); down.consume(); return@awaitEachGesture }
                        if (longPressOnly) {
                            // Own the touch stream before the clickable below or a parent bubble sees it.
                            // The clickable still supplies keyboard and accessibility actions.
                            down.consume()
                            val press = PressInteraction.Press(down.position)
                            pressSource.tryEmit(press)
                            var released = false
                            try {
                                awaitPointerEvent(PointerEventPass.Final)
                                val held = awaitLongPressOrCancellation(down.id)
                                if (held == null) {
                                    val up = currentEvent.changes.firstOrNull { it.id == down.id }
                                    if (up != null && !up.pressed && !up.isConsumed &&
                                        (up.position - down.position).getDistance() <= viewConfiguration.touchSlop
                                    ) {
                                        up.consume()
                                        pressSource.tryEmit(PressInteraction.Release(press))
                                        released = true
                                        click?.invoke()
                                    }
                                    return@awaitEachGesture
                                }
                                if ((held.position - down.position).getDistance() > viewConfiguration.touchSlop) {
                                    return@awaitEachGesture
                                }
                                haptic.performHapticFeedback(HapticFeedbackType.LongPress)
                            } finally {
                                if (!released) pressSource.tryEmit(PressInteraction.Cancel(press))
                            }
                        }
                        expanded = true; hovered = -1
                        if (touchExploration) { tapHeld = true; return@awaitEachGesture }
                        val dragged = dragSelect(down, haptic, ::indexAt, { hovered = it }) {
                            flat.getOrNull(it)?.onClick()
                            close()
                        }
                        if (!dragged) tapHeld = true
                    }
                }
                .then(if (onClick == null) Modifier else Modifier.combinedClickable(
                    interactionSource = pressSource,
                    indication = indication,
                    role = Role.Button,
                    onClickLabel = onClickLabel,
                    onLongClickLabel = onLongClickLabel,
                    onLongClick = { expanded = true; tapHeld = true },
                    onClick = { click?.invoke() },
                ))
        ) { anchor() }

        if (vis.currentState || vis.targetState) {
            // The popup window is padded so the card's shadow isn't cut at the window's edge.
            val offXpx = with(density) { offset.x.roundToPx() }
            val offYpx = with(density) { (offset.y - PopupPad.calculateTopPadding()).roundToPx() }
            Popup(
                alignment = Alignment.TopEnd,
                offset = IntOffset(offXpx, offYpx),
                onDismissRequest = { close() },
                // A focusable window opening mid-gesture cancels the drag. Once a tap holds
                // the menu open, focusable enables outside-tap dismiss.
                properties = PopupProperties(focusable = tapHeld),
            ) {
                AnimatedVisibility(
                    visibleState = vis,
                    enter = scaleIn(tween(120), 0.85f, TransformOrigin(1f, 0f)) + fadeIn(tween(120)),
                    exit = scaleOut(tween(100), 0.85f, TransformOrigin(1f, 0f)) + fadeOut(tween(100)),
                ) {
                    MenuCard(
                        groups = groups,
                        hovered = hovered,
                        modifier = Modifier.pointerInput(Unit) { detectTapGestures { close() } }.padding(PopupPad),
                        onRowPositioned = { i, c -> rows[i] = Rect(c.positionOnScreen(), c.size.toSize()) },
                        itemHeight = itemHeight,
                        verticalPadding = verticalPadding,
                        shape = shape,
                        iconSize = iconSize,
                        onPick = { action -> action.onClick(); close() },
                    )
                }
            }
        }
    }
}

/**
 * The finger that opened a menu drives it: dragging hovers [hitAt] targets, and a lift after a drag hands
 * [onRelease] the one under it, or -1. Returns false for a lift without a drag, which leaves the menu open.
 */
internal suspend fun AwaitPointerEventScope.dragSelect(
    down: PointerInputChange,
    haptic: HapticFeedback,
    hitAt: (Offset) -> Int,
    onHover: (Int) -> Unit,
    onRelease: (Int) -> Unit,
): Boolean {
    var dragged = false
    var hovered = -1
    while (true) {
        val ch = awaitPointerEvent().changes.let { all -> all.firstOrNull { it.id == down.id } ?: all.first() }
        if (!ch.pressed) {
            if (dragged) {
                val hit = hitAt(ch.position)
                onHover(-1)
                if (hit >= 0) haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                onRelease(hit)
            }
            return dragged
        }
        if (!dragged && (ch.position - down.position).getDistance() > viewConfiguration.touchSlop) dragged = true
        if (dragged) {
            val hit = hitAt(ch.position)
            if (hit != hovered) {
                hovered = hit
                onHover(hit)
                if (hit >= 0) haptic.performHapticFeedback(HapticFeedbackType.SegmentTick)
            }
        }
        ch.consume()
    }
}

/** Shared by the pressed item, which owns the pointer stream, and the overlay, which owns the visuals. */
open class DragMenuState<A : Any>(maxTargets: Int) {
    var anchor by mutableStateOf<A?>(null)
        private set
    internal var closing by mutableStateOf(false)

    /** Index into [targets], -1 for none. */
    internal var hovered by mutableIntStateOf(-1)
    internal val targets = arrayOfNulls<LayoutCoordinates>(maxTargets)
    internal val scrim = Animatable(0f)
    internal val pop = Animatable(0f)

    val isOpen get() = anchor != null

    fun open(anchor: A) {
        if (isOpen) return
        targets.fill(null)
        hovered = -1
        closing = false
        this.anchor = anchor
    }

    /** Plays the exit; the anchor releases when it finishes. */
    fun close() {
        if (isOpen) closing = true
    }

    internal fun closed() {
        anchor = null
        closing = false
        hovered = -1
    }

    /** The target under [local], a position in [from]. Bounds are read live so animated transforms count. */
    internal fun hitIndex(from: LayoutCoordinates?, local: Offset): Int {
        if (closing) return -1
        val at = from?.takeIf { it.isAttached }?.localToRoot(local) ?: return -1
        return targets.indexOfFirst { c ->
            c != null && c.isAttached && Rect(c.positionInRoot(), c.size.toSize()).contains(at)
        }
    }
}

private val PopupPad = PaddingValues(start = 24.dp, end = 8.dp, top = 8.dp, bottom = 32.dp)

internal val Overshoot = Easing { OvershootInterpolator(1.1f).getInterpolation(it) }

/** Dims and guards the screen behind a [DragMenuState] menu, and plays the menu's pop in and out. */
@Composable
internal fun MenuBackdrop(state: DragMenuState<*>, dim: Float, scrimMs: Int, popMs: Int, exitMs: Int) {
    androidx.activity.compose.BackHandler(!state.closing) { state.close() }
    LaunchedEffect(Unit) {
        launch { state.scrim.animateTo(dim, tween(scrimMs, easing = EaseOutQuint)) }
        state.pop.animateTo(1f, tween(popMs, easing = Overshoot))
    }
    LaunchedEffect(state.closing) {
        if (state.closing) {
            launch { state.scrim.animateTo(0f, tween(exitMs)) }
            state.pop.animateTo(0f, tween(exitMs))
            state.closed()
        }
    }
    Box(
        Modifier
            .fillMaxSize()
            .drawBehind { drawRect(Color.Black.copy(alpha = state.scrim.value)) }
            .pointerInput(Unit) { detectTapGestures { state.close() } },
    )
}

/** [hovered] is a flat index across all groups, -1 for none. */
@Composable
fun MenuCard(
    groups: List<List<MenuAction>>,
    hovered: Int,
    modifier: Modifier = Modifier,
    itemHeight: Dp = 48.dp,
    verticalPadding: Dp = 0.dp,
    shape: Shape = RoundedCornerShape(Radii.menu),
    iconSize: Dp = MenuIconSize,
    haze: HazeState? = LocalRootHaze.current,
    onRowPositioned: ((Int, LayoutCoordinates) -> Unit)? = null,
    onPick: (MenuAction) -> Unit,
) {
    Box(modifier.floatingSurface(shape, haze)) {
        androidx.compose.runtime.CompositionLocalProvider(LocalOnGlass provides true) {
        Column(Modifier.width(IntrinsicSize.Max).padding(vertical = verticalPadding)) {
            var i = 0
            groups.forEachIndexed { gi, group ->
                if (gi != 0) HorizontalDivider(color = Promtuz.colors.divider)
                group.forEach { action ->
                    val index = i
                    MenuRow(
                        action,
                        itemHeight,
                        iconSize,
                        hovered == index,
                        Modifier.let { m ->
                            if (onRowPositioned == null) m
                            else m.onGloballyPositioned { onRowPositioned(index, it) }
                        },
                    ) { onPick(action) }
                    i++
                }
            }
        }
    }
    }
}

@Composable
private fun MenuRow(
    action: MenuAction,
    itemHeight: Dp,
    iconSize: Dp,
    highlighted: Boolean,
    modifier: Modifier = Modifier,
    onClick: () -> Unit,
) {
    val color = if (action.destructive) Promtuz.colors.danger else Promtuz.colors.text
    Row(
        modifier
            .fillMaxWidth()
            .widthIn(min = 160.dp, max = 280.dp)
            .height(itemHeight)
            .background(
                if (highlighted) Promtuz.colors.glassFill else Color.Transparent
            )
            .clickable(role = Role.Button, onClickLabel = action.label, onClick = onClick)
            .padding(horizontal = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        when {
            action.glyph != null -> MorphIcon(action.glyph, null, Modifier.size(iconSize), tint = color)
            action.icon != null -> DrawableIcon(action.icon, tint = color, size = iconSize)
            action.iconPlaceholder != null -> Text("[${action.iconPlaceholder}]", color = color,
                style = MaterialTheme.typography.labelSmall)
        }
        Text(action.label, color = color, style = MenuLabelStyle)
    }
}

private val MenuLabelStyle = TextStyle(fontSize = 15.sp, lineHeight = 20.sp, fontWeight = FontWeight.Medium)
