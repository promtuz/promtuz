package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Radii
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.tween
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.isSpecified
import androidx.compose.ui.util.lerp
import androidx.compose.foundation.IndicationNodeFactory
import androidx.compose.foundation.indication
import androidx.compose.foundation.interaction.InteractionSource
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.PressInteraction
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ButtonColors
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.IconButtonColors
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.minimumInteractiveComponentSize
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.Alignment
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.drawscope.ContentDrawScope
import androidx.compose.ui.graphics.drawscope.clipRect
import androidx.compose.ui.node.CompositionLocalConsumerModifierNode
import androidx.compose.ui.node.DelegatableNode
import androidx.compose.ui.node.DrawModifierNode
import androidx.compose.ui.node.currentValueOf
import androidx.compose.ui.node.invalidateDraw
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.theme.LocalPromtuzColors
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * The app's ripple. Like Telegram's, it spreads from the finger *behind* the content, the way an
 * Android view's background ripple does, so text and icons stay crisp; Material's paints over them.
 * It is faint, and a quick tap still finishes its spread before fading.
 */
object PressHighlight : IndicationNodeFactory {
    override fun create(interactionSource: InteractionSource): DelegatableNode = PressNode(interactionSource)
    override fun equals(other: Any?) = other === this
    override fun hashCode() = 7
}

// Android's own ripple timing: a 75ms fade-in, a 225ms spread from 30% of the radius, and a
// 150ms fade-out once released. Only the wave shows; its feathered rim keeps the edge soft.
private class PressNode(private val source: InteractionSource) :
    Modifier.Node(), DrawModifierNode, CompositionLocalConsumerModifierNode {
    private val spread = Animatable(0f)
    private val alpha = Animatable(0f)
    private var origin = Offset.Unspecified
    private var job: Job? = null

    override fun onAttach() {
        coroutineScope.launch {
            source.interactions.collect { i ->
                when (i) {
                    is PressInteraction.Press -> {
                        job?.cancel()
                        origin = i.pressPosition
                        spread.snapTo(0f)
                        job = launch {
                            launch { alpha.animateTo(1f, tween(FADE_IN_MS, easing = LinearEasing)) { invalidateDraw() } }
                            spread.animateTo(1f, tween(SPREAD_MS, easing = FastOutSlowInEasing)) { invalidateDraw() }
                        }
                    }
                    is PressInteraction.Release, is PressInteraction.Cancel -> {
                        val pressing = job
                        job = launch {
                            pressing?.join()
                            alpha.animateTo(0f, tween(FADE_OUT_MS, easing = LinearEasing)) { invalidateDraw() }
                        }
                    }
                }
            }
        }
    }

    override fun ContentDrawScope.draw() {
        val a = alpha.value
        if (a > 0f && origin.isSpecified) {
            val c = currentValueOf(LocalPromtuzColors)
            val tint = c.text.copy(alpha = (if (c.isDark) 0.07f else 0.06f) * a)
            val far = maxOf(
                origin.getDistance(), (origin - Offset(size.width, 0f)).getDistance(),
                (origin - Offset(0f, size.height)).getDistance(), (origin - Offset(size.width, size.height)).getDistance(),
            )
            val r = lerp(far * 0.3f, far * 1.15f, spread.value)
            // A feathered rim, so the wave brightens the row rather than drawing a disc's outline.
            // Bounded to the pressed control: a row's wave never spills onto its neighbours.
            clipRect {
                drawCircle(
                    Brush.radialGradient(0f to tint, 0.7f to tint, 1f to tint.copy(alpha = 0f), center = origin, radius = r),
                    r, origin,
                )
            }
        }
        drawContent()
    }

    private companion object { const val SPREAD_MS = 225; const val FADE_IN_MS = 75; const val FADE_OUT_MS = 150 }
}

/** Text buttons in a dialog's footer round to follow the dialog's corner. */
val LocalTextButtonShape = androidx.compose.runtime.staticCompositionLocalOf<Shape> { RoundedCornerShape(Radii.small) }

// Material's buttons under the app's names, so every call site gets the app's ripple.

@Composable
fun TextButton(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    colors: ButtonColors = ButtonDefaults.textButtonColors(),
    contentPadding: PaddingValues = ButtonDefaults.TextButtonContentPadding,
    content: @Composable RowScope.() -> Unit,
) {
    val source = remember { MutableInteractionSource() }
    // The touch target keeps Material's 48dp; the highlight stays on the 40dp button.
    Box(
        modifier.minimumInteractiveComponentSize()
            .clickable(source, null, enabled, role = Role.Button, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Row(
            Modifier.defaultMinSize(58.dp, 40.dp).clip(LocalTextButtonShape.current).indication(source, PressHighlight)
                .padding(contentPadding),
            horizontalArrangement = Arrangement.Center,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            CompositionLocalProvider(
                LocalContentColor provides if (enabled) colors.contentColor else colors.disabledContentColor,
                LocalTextStyle provides MaterialTheme.typography.labelLarge,
            ) { content() }
        }
    }
}

@Composable
fun IconButton(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    colors: IconButtonColors = IconButtonDefaults.iconButtonColors(),
    content: @Composable () -> Unit,
) {
    val source = remember { MutableInteractionSource() }
    Box(
        modifier.minimumInteractiveComponentSize()
            .clickable(source, null, enabled, role = Role.Button, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            Modifier.size(40.dp).clip(CircleShape).background(if (enabled) colors.containerColor else colors.disabledContainerColor)
                .indication(source, PressHighlight),
            contentAlignment = Alignment.Center,
        ) {
            CompositionLocalProvider(
                LocalContentColor provides if (enabled) colors.contentColor else colors.disabledContentColor,
                content = content,
            )
        }
    }
}
