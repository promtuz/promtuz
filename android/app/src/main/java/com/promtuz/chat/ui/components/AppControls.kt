package com.promtuz.chat.ui.components

import androidx.annotation.DrawableRes
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateDpAsState
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.Animatable
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalHapticFeedback
import kotlinx.coroutines.launch
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.LocalTextStyle
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.Radii

/** The radius for a shape sitting [inset] inside one of radius [outer]. */
fun concentric(outer: Dp, inset: Dp, min: Dp = 4.dp): Dp = (outer - inset).coerceAtLeast(min)

enum class ButtonKind { Primary, Tonal, Danger }

private val ButtonText = TextStyle(fontSize = 16.sp, lineHeight = 22.sp, fontWeight = FontWeight.Medium)

/** Ripples like every other control; a busy button keeps its size and swaps the label for a spinner. */
@Composable
fun AppButton(
    text: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    kind: ButtonKind = ButtonKind.Primary,
    @DrawableRes icon: Int? = null,
    enabled: Boolean = true,
    busy: Boolean = false,
    shape: Shape = RoundedCornerShape(Radii.button),
) = AppButton(onClick, modifier, kind, enabled, busy, shape) {
    icon?.let { DrawableIcon(it, size = 20.dp, tint = LocalContentColor.current) }
    Text(text)
}

@Composable
fun AppButton(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    kind: ButtonKind = ButtonKind.Primary,
    enabled: Boolean = true,
    busy: Boolean = false,
    shape: Shape = RoundedCornerShape(Radii.button),
    content: @Composable RowScope.() -> Unit,
) {
    val c = Promtuz.colors
    val (bg, fg) = when {
        !enabled -> c.switchOff to c.muted
        kind == ButtonKind.Primary -> c.accentFill to Color.White
        kind == ButtonKind.Tonal -> raisedFill() to c.accent
        else -> c.danger.copy(alpha = 0.14f) to c.danger
    }
    val background by animateColorAsState(bg, label = "buttonBg")
    val interaction = remember { MutableInteractionSource() }
    Row(
        modifier
            .clip(shape)
            .background(background)
            .clickable(interaction, PressHighlight, enabled && !busy, role = Role.Button, onClick = onClick)
            .heightIn(min = 52.dp)
            .padding(horizontal = 24.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp, Alignment.CenterHorizontally),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (busy) CircularProgressIndicator(Modifier.size(20.dp), color = fg, strokeWidth = 2.dp)
        else CompositionLocalProvider(LocalContentColor provides fg, LocalTextStyle provides ButtonText) { content() }
    }
}

private val SwitchW = 38.dp
private val SwitchH = 22.dp
private val SwitchPad = 3.dp
private val Thumb = 16.dp
private val ThumbStretch = 6.dp
private val SwitchRadius = 10.dp

/**
 * A rounded-rectangle switch that answers the finger: the thumb stretches while held, can be
 * dragged across, and the track's colour follows the thumb. Nothing is painted over it.
 *
 * In a row that toggles as a whole, pass the row's [interactionSource] with [tappable] false, so
 * pressing the row stretches the thumb and the row keeps the tap while the switch keeps the drag.
 */
@Composable
fun AppSwitch(
    checked: Boolean,
    onCheckedChange: ((Boolean) -> Unit)?,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    interactionSource: MutableInteractionSource? = null,
    tappable: Boolean = true,
) {
    val c = Promtuz.colors
    val source = interactionSource ?: remember { MutableInteractionSource() }
    val pressed by source.collectIsPressedAsState()
    var dragging by remember { mutableStateOf(false) }
    val progress = remember { Animatable(if (checked) 1f else 0f) }
    val scope = rememberCoroutineScope()
    val haptics = LocalHapticFeedback.current
    val sign = if (androidx.compose.ui.platform.LocalLayoutDirection.current == androidx.compose.ui.unit.LayoutDirection.Rtl) -1 else 1
    val settle = spring<Float>(dampingRatio = 0.72f, stiffness = 520f)
    LaunchedEffect(checked) { if (!dragging) progress.animateTo(if (checked) 1f else 0f, settle) }
    val stretch by animateFloatAsState(if ((pressed || dragging) && enabled) 1f else 0f,
        spring(dampingRatio = 0.8f, stiffness = 600f), label = "switchStretch")
    val set: (Boolean) -> Unit = { v ->
        haptics.performHapticFeedback(if (v) HapticFeedbackType.ToggleOn else HapticFeedbackType.ToggleOff)
        onCheckedChange?.invoke(v)
    }

    val gestures = if (onCheckedChange == null || !enabled) Modifier else Modifier
        .then(if (tappable) Modifier.toggleable(checked, source, null, enabled, Role.Switch) { set(it) } else Modifier)
        .pointerInput(checked) {
            val travel = (SwitchW - SwitchPad * 2 - Thumb - ThumbStretch).toPx()
            detectHorizontalDragGestures(
                onDragStart = { dragging = true },
                onDragEnd = {
                    dragging = false
                    val on = progress.value > 0.5f
                    if (on != checked) set(on) else scope.launch { progress.animateTo(if (checked) 1f else 0f, settle) }
                },
                onDragCancel = {
                    dragging = false
                    scope.launch { progress.animateTo(if (checked) 1f else 0f, settle) }
                },
            ) { change, dx ->
                change.consume()
                scope.launch { progress.snapTo((progress.value + sign * dx / travel).coerceIn(0f, 1f)) }
            }
        }

    Box(
        modifier
            .then(gestures)
            .size(SwitchW, SwitchH)
            .graphicsLayer { alpha = if (enabled) 1f else 0.38f }
            .drawBehind {
                val p = progress.value
                val r = SwitchRadius.toPx()
                drawRoundRect(lerp(c.switchOff, c.accentFill, p), cornerRadius = CornerRadius(r))
                val pad = SwitchPad.toPx()
                val w = (Thumb + ThumbStretch * stretch).toPx()
                val x = pad + (size.width - pad * 2 - w) * p
                val left = if (layoutDirection == androidx.compose.ui.unit.LayoutDirection.Rtl) size.width - x - w else x
                drawRoundRect(
                    lerp(c.thumbOff, Color.White, p),
                    topLeft = Offset(left, pad),
                    size = Size(w, Thumb.toPx()),
                    cornerRadius = CornerRadius(concentric(SwitchRadius, SwitchPad).toPx()),
                )
            },
    )
}

/** A 24 glyph on the app's [IconButton]. */
@Composable
fun AppIconButton(
    @DrawableRes icon: Int,
    contentDescription: String?,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    tint: Color = Promtuz.colors.text,
    enabled: Boolean = true,
) {
    IconButton(onClick, modifier, enabled) {
        DrawableIcon(icon, desc = contentDescription.orEmpty(), size = 24.dp, tint = tint)
    }
}

/**
 * A 4dp track with a rounded-rectangle thumb. Tap or drag anywhere on it; the thumb swells while held.
 * [steps] snaps to that many evenly spaced values between the ends (0 is continuous).
 */
@Composable
fun AppSlider(
    value: Float,
    onValueChange: (Float) -> Unit,
    modifier: Modifier = Modifier,
    range: ClosedFloatingPointRange<Float> = 0f..1f,
    steps: Int = 0,
    enabled: Boolean = true,
) {
    val c = Promtuz.colors
    var held by remember { mutableStateOf(false) }
    val grow by animateFloatAsState(if (held) 1f else 0f, spring(dampingRatio = 0.7f, stiffness = 700f), label = "sliderGrow")
    val rtl = androidx.compose.ui.platform.LocalLayoutDirection.current == androidx.compose.ui.unit.LayoutDirection.Rtl
    val fraction = ((value - range.start) / (range.endInclusive - range.start)).coerceIn(0f, 1f)
    val thumb = 20.dp
    // The gesture outlives recompositions, so it must call the latest callback.
    val onChange by rememberUpdatedState(onValueChange)
    fun pick(x: Float, width: Float, inset: Float) {
        var f = ((x - inset) / (width - inset * 2)).coerceIn(0f, 1f)
        if (rtl) f = 1f - f
        if (steps > 0) f = Math.round(f * (steps + 1)) / (steps + 1f)
        onChange(range.start + f * (range.endInclusive - range.start))
    }
    Box(
        modifier
            .fillMaxWidth()
            .height(28.dp)
            // A tap jumps the thumb, a sideways drag moves it, and a vertical drag is left to the page.
            .then(if (!enabled) Modifier else Modifier.pointerInput(range, steps, rtl) {
                val inset = thumb.toPx() / 2
                val slop = viewConfiguration.touchSlop
                awaitEachGesture {
                    val down = awaitFirstDown()
                    held = true
                    var dragging = false
                    while (true) {
                        val ch = awaitPointerEvent().changes.firstOrNull { it.id == down.id } ?: break
                        if (!ch.pressed) {
                            if (!dragging && !ch.isConsumed) pick(ch.position.x, size.width.toFloat(), inset)
                            break
                        }
                        if (ch.isConsumed) break
                        val d = ch.position - down.position
                        if (!dragging) {
                            if (kotlin.math.abs(d.y) > slop && kotlin.math.abs(d.y) > kotlin.math.abs(d.x)) break
                            if (kotlin.math.abs(d.x) > slop) dragging = true
                        }
                        if (dragging) { pick(ch.position.x, size.width.toFloat(), inset); ch.consume() }
                    }
                    held = false
                }
            })
            .graphicsLayer { alpha = if (enabled) 1f else 0.38f }
            .drawBehind {
                val inset = thumb.toPx() / 2
                val cy = size.height / 2
                val track = 4.dp.toPx()
                val span = size.width - inset * 2
                val x = inset + span * (if (rtl) 1f - fraction else fraction)
                drawRoundRect(c.switchOff, Offset(inset, cy - track / 2), Size(span, track), CornerRadius(track / 2))
                val (a, b) = if (rtl) x to size.width - inset else inset to x
                drawRoundRect(c.accentFill, Offset(a, cy - track / 2), Size(b - a, track), CornerRadius(track / 2))
                val w = (thumb + 6.dp * grow).toPx()
                val r = concentric(10.dp, 3.dp).toPx() + 2.dp.toPx() * grow
                drawRoundRect(c.card, Offset(x - w / 2 - 3.dp.toPx(), cy - w / 2 - 3.dp.toPx()), Size(w + 6.dp.toPx(), w + 6.dp.toPx()), CornerRadius(r + 3.dp.toPx()))
                drawRoundRect(c.accentFill, Offset(x - w / 2, cy - w / 2), Size(w, w), CornerRadius(r))
            },
    )
}

/** A row of mutually exclusive tabs on a card, the selected one raised. */
@Composable
fun SegmentedTabs(labels: List<String>, selected: Int, modifier: Modifier = Modifier, onSelect: (Int) -> Unit) {
    val c = Promtuz.colors
    val slab = rememberSlabState()
    Row(modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(cardFill())
        .slabHost(slab, selected, labels.size, raisedFill(), concentric(Radii.card, 4.dp), onSelect).padding(4.dp)) {
        labels.forEachIndexed { i, label ->
            val on = slab.shown(selected) == i
            Box(
                Modifier.weight(1f).height(36.dp).slabOption(slab, i, i == selected, Role.Tab) { onSelect(i) },
                contentAlignment = Alignment.Center,
            ) {
                Text(label, style = TextStyle(fontSize = 14.sp, lineHeight = 20.sp, fontWeight = if (on) FontWeight.Medium else FontWeight.Normal),
                    color = if (on) c.text else c.muted)
            }
        }
    }
}
