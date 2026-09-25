package com.promtuz.chat.ui.animation

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedContentScope
import androidx.compose.animation.AnimatedContentTransitionScope
import androidx.compose.animation.ContentTransform
import androidx.compose.animation.SizeTransform
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.rememberTransition
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import com.promtuz.chat.ui.constants.Tweens

/**
 * Finish the visible transition before advancing, even when [targetState] changes
 * several times in one animation. Keep only the latest destination, not stale requests.
 *
 * Labels use a full [durationMillis] hop, followed by a [minDurationMillis] catch-up
 * hop when another request arrived mid-animation (the original home-title behavior).
 * With [ContentProgression.Integers], every intermediate integer is shown. Each hop
 * takes duration / remaining steps, bounded by [minDurationMillis]; reversing or
 * extending the destination takes effect after the active hop completes.
 *
 * Render the value passed to [content], not the external target. [transitionSpec]
 * receives a duration fixed for that hop; use it for size changes as well as entry
 * and exit. Defaults to the app's vertical slide/fade, with matching size timing.
 * Values use equality, support null, and should be immutable. Use Compose `key`
 * around this component when switching to an unrelated stream of values.
 */
@Composable
fun <T> QueuedAnimatedContent(
    targetState: T,
    modifier: Modifier = Modifier,
    durationMillis: Int = 300,
    minDurationMillis: Int = minOf(80, durationMillis),
    progression: ContentProgression<T>? = null,
    contentAlignment: Alignment = Alignment.CenterStart,
    label: String = "queued content",
    transitionSpec: AnimatedContentTransitionScope<T>.(durationMillis: Int) -> ContentTransform = {
        queuedVerticalTransition(it)
    },
    content: @Composable AnimatedContentScope.(T) -> Unit,
) {
    require(durationMillis > 0 && minDurationMillis in 1..durationMillis)
    val state = remember { MutableTransitionState(targetState) }
    val queue = remember { ContentQueue(targetState) }
    val requested by rememberUpdatedState(targetState)
    val baseDuration by rememberUpdatedState(durationMillis)
    val minimumDuration by rememberUpdatedState(minDurationMillis)
    val currentProgression by rememberUpdatedState(progression)
    var hopDuration by remember { mutableIntStateOf(durationMillis) }
    val transition = rememberTransition(state, label)

    // One collector per mounted component. Read only target changes and the idle
    // boundary, so animation frames do not drive queue work or recomposition.
    LaunchedEffect(state) {
        // Include the settled value: with animations disabled, Compose can pass
        // through busy -> idle before the collector observes the busy snapshot.
        snapshotFlow { Triple(requested, state.isIdle, state.currentState) }.collect { (value, idle, _) ->
            queue.offer(value)
            if (idle) {
                queue.complete()
                queue.next(baseDuration, minimumDuration, currentProgression)?.let { hop ->
                    hopDuration = hop.durationMillis
                    state.targetState = hop.target
                }
            }
        }
    }

    transition.AnimatedContent(
        modifier = modifier,
        contentAlignment = contentAlignment,
        transitionSpec = { transitionSpec(hopDuration) },
        content = content,
    )
}

/** Shared entry, exit and size timing; no delayed fade that exposes a blank label. */
fun <T> AnimatedContentTransitionScope<T>.queuedVerticalTransition(durationMillis: Int, clip: Boolean = true): ContentTransform =
    ((slideInVertically(Tweens.microInteraction(durationMillis)) { it } + fadeIn(Tweens.microInteraction(durationMillis))) togetherWith
        (slideOutVertically(Tweens.microInteraction(durationMillis)) { -it } + fadeOut(Tweens.microInteraction(durationMillis))))
        .using(SizeTransform(clip = clip, sizeAnimationSpec = { _, _ -> Tweens.microInteraction(durationMillis) }))
