package com.promtuz.chat.ui.components

import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.Dp
import com.promtuz.chat.R
import com.promtuz.chat.ui.theme.Promtuz
import androidx.compose.animation.*
import androidx.compose.animation.core.*
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.key.*
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.delay

const val PIN_LENGTH = 6

/** Typed PIN entries; the newest shows its digit briefly before turning into a dot, as in Telegram. */
@Composable
fun CodeDots(code: String, checking: Boolean, modifier: Modifier = Modifier, color: Color = MaterialTheme.colorScheme.onSurface) {
    var reveal by remember { mutableIntStateOf(-1) }
    var last by remember { mutableIntStateOf(code.length) }
    LaunchedEffect(code.length) {
        reveal = if (code.length > last) code.length - 1 else -1
        last = code.length
        if (reveal >= 0) { delay(1500); reveal = -1 }
    }
    val wave = rememberInfiniteTransition(label = "checking")
    val phase by wave.animateFloat(0f, 1f, infiniteRepeatable(tween(900, easing = LinearEasing)), label = "phase")
    val lift = with(LocalDensity.current) { 20.dp.roundToPx() }
    Row(modifier.height(36.dp).semantics { contentDescription = "${code.length} of $PIN_LENGTH digits" },
        verticalAlignment = Alignment.CenterVertically) {
        repeat(PIN_LENGTH) { i ->
            AnimatedVisibility(
                visible = i < code.length,
                enter = fadeIn(tween(150)) + scaleIn(tween(150)) + slideIn(tween(150)) { IntOffset(0, lift) } +
                    expandHorizontally(tween(150), expandFrom = Alignment.CenterHorizontally),
                exit = fadeOut(tween(150)) + scaleOut(tween(150)) +
                    shrinkHorizontally(tween(150), shrinkTowards = Alignment.CenterHorizontally),
            ) {
                val pulse = if (checking) 0.35f + 0.65f * (1f - ((phase * PIN_LENGTH - i).mod(PIN_LENGTH.toFloat()) / 2f).coerceIn(0f, 1f)) else 1f
                Box(Modifier.width(30.dp).alpha(pulse), contentAlignment = Alignment.Center) {
                    Crossfade(i == reveal, animationSpec = tween(150), label = "reveal") { shown ->
                        if (shown) Text(code.getOrNull(i)?.toString().orEmpty(), color = color,
                            fontSize = 26.sp, fontWeight = FontWeight.Bold)
                        else Box(Modifier.size(12.dp).clip(CircleShape).background(color))
                    }
                }
            }
        }
    }
}

/** Six boxes for choosing a PIN; [filled] turns them to the accent one after another. */
@Composable
fun CodeBoxes(code: String, filled: Boolean, modifier: Modifier = Modifier) {
    val colors = MaterialTheme.colorScheme
    Row(modifier.semantics { contentDescription = "${code.length} of $PIN_LENGTH digits" },
        horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        repeat(PIN_LENGTH) { i ->
            val fill by animateColorAsState(if (filled) colors.primary else colors.surfaceContainerHigh,
                tween(200, delayMillis = if (filled) i * 75 else 0), label = "fill")
            val active = !filled && i == code.length
            Box(
                Modifier.size(44.dp, 52.dp).clip(RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.small)).background(fill)
                    .border(if (active) 2.dp else 0.dp, if (active) colors.primary else Color.Transparent, RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.small)),
                contentAlignment = Alignment.Center,
            ) {
                androidx.compose.animation.AnimatedVisibility(i < code.length, enter = scaleIn(tween(150)) + fadeIn(tween(150)),
                    exit = scaleOut(tween(150)) + fadeOut(tween(150))) {
                    Box(Modifier.size(12.dp).clip(CircleShape).background(if (filled) colors.onPrimary else colors.onSurface))
                }
            }
        }
    }
}

private val KEY_LETTERS = listOf("+", " ", "ABC", "DEF", "GHI", "JKL", "MNO", "PQRS", "TUV", "WXYZ")

/** Rounded keys whose outer grid corners open up a little, so the filled block reads as one shape. */
@Composable
fun CodeKeypad(
    onDigit: (Char) -> Unit,
    onBackspace: () -> Unit,
    onClear: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val haptic = LocalHapticFeedback.current
    val c = Promtuz.colors
    val compact = LocalConfiguration.current.screenHeightDp < 600
    val key = if (compact) 58.dp else 72.dp
    val r = if (compact) 16.dp else 20.dp
    val outer = if (compact) 21.dp else 26.dp
    val fill = c.text.copy(alpha = if (c.isDark) 0.08f else 0.06f)
    Column(modifier, verticalArrangement = Arrangement.spacedBy(12.dp), horizontalAlignment = Alignment.CenterHorizontally) {
        listOf("123", "456", "789", " 0<").forEach { row ->
            Row(horizontalArrangement = Arrangement.spacedBy(16.dp)) {
                row.forEach { k ->
                    val shape = when (k) {
                        '1' -> RoundedCornerShape(outer, r, r, r)
                        '3' -> RoundedCornerShape(r, outer, r, r)
                        '7' -> RoundedCornerShape(r, r, r, outer)
                        '9' -> RoundedCornerShape(r, r, outer, r)
                        '0' -> RoundedCornerShape(r, r, outer, outer)
                        else -> RoundedCornerShape(r)
                    }
                    when (k) {
                        ' ' -> Spacer(Modifier.size(key))
                        '<' -> PadKey(key, CircleShape, Color.Transparent, "Delete", {
                            haptic.performHapticFeedback(HapticFeedbackType.KeyboardTap); onBackspace()
                        }, { haptic.performHapticFeedback(HapticFeedbackType.LongPress); onClear() }) {
                            DrawableIcon(R.drawable.i_backspace, tint = c.text, size = if (compact) 22.dp else 27.dp)
                        }
                        else -> PadKey(key, shape, fill, k.toString(), {
                            haptic.performHapticFeedback(HapticFeedbackType.KeyboardTap); onDigit(k)
                        }) {
                            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                                Text(k.toString(), color = c.text, style = if (compact) DigitCompact else Digit)
                                Text(KEY_LETTERS[k - '0'], color = c.muted, style = Letters,
                                    letterSpacing = if (k == '0') 1.2.sp else 0.sp)
                            }
                        }
                    }
                }
            }
        }
    }
}

private val Digit = TextStyle(fontSize = 30.sp, lineHeight = 33.sp)
private val DigitCompact = TextStyle(fontSize = 24.sp, lineHeight = 27.sp)
private val Letters = TextStyle(fontSize = 10.sp, lineHeight = 12.sp, fontWeight = FontWeight.Medium)

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun PadKey(
    size: Dp, shape: Shape, color: Color, label: String,
    onClick: () -> Unit, onLongClick: (() -> Unit)? = null, content: @Composable () -> Unit,
) {
    val source = remember { MutableInteractionSource() }
    val pressed by source.collectIsPressedAsState()
    val scale by animateFloatAsState(if (pressed) 0.9f else 1f, tween(100), label = "press")
    Box(
        Modifier.size(size).graphicsLayer { scaleX = scale; scaleY = scale }.clip(shape).background(color)
            .combinedClickable(source, PressHighlight, onClickLabel = label, onLongClick = onLongClick, onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) { content() }
}

/** Hardware and adb digit keys for a focused keypad screen. */
fun Modifier.codeKeys(onDigit: (Char) -> Unit, onBackspace: () -> Unit): Modifier = onKeyEvent { e ->
    if (e.type != KeyEventType.KeyDown) return@onKeyEvent false
    val c = e.utf16CodePoint.toChar()
    when {
        c in '0'..'9' -> { onDigit(c); true }
        e.key == Key.Backspace -> { onBackspace(); true }
        else -> false
    }
}

/** Telegram's wrong-code shake: a 600-stiffness spring kicked sideways, each kick reversing the last. */
@Composable
fun Modifier.shake(trigger: Int): Modifier {
    val offset = remember { Animatable(0f) }
    val shift = with(LocalDensity.current) { 10.dp.toPx() }
    LaunchedEffect(trigger) {
        if (trigger == 0) return@LaunchedEffect
        offset.snapTo(0f)
        val direction = if (trigger % 2 == 0) 1 else -1
        offset.animateTo(0f, spring(dampingRatio = 0.5f, stiffness = 600f), initialVelocity = -shift * 100 * direction)
    }
    return graphicsLayer { translationX = offset.value }
}
