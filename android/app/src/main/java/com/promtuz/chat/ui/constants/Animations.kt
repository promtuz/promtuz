package com.promtuz.chat.ui.constants

import androidx.compose.animation.ContentTransform
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.EaseInOutCirc
import androidx.compose.animation.core.TweenSpec
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith

object Tweens {
    fun <T> microInteraction(dur: Int = 150): TweenSpec<T> {
        return tween(dur, easing = EaseInOutCirc)
    }
}

object Buttonimations {
    fun labelSlide(): ContentTransform {
        return (slideInVertically(
            initialOffsetY = { fullHeight -> fullHeight }, animationSpec = Tweens.microInteraction()
        ) + fadeIn(Tweens.microInteraction())) togetherWith (slideOutVertically(
            targetOffsetY = { fullHeight -> -fullHeight }, animationSpec = Tweens.microInteraction()
        ) + fadeOut(Tweens.microInteraction()))
    }
}

object Naviganimation {
    private val enter = CubicBezierEasing(0.2f, 0.8f, 0.2f, 1f)
    private val exit = CubicBezierEasing(0.2f, 0.8f, 0.2f, 0.9f)

    fun transitionSpec() = ContentTransform(
        targetContentEnter = fadeIn(tween(500, easing = enter)) +
                slideInHorizontally(tween(500, easing = enter)) { it / 4 },
        initialContentExit = fadeOut(tween(500, easing = exit)) +
                slideOutHorizontally(tween(500, easing = exit)) { -it / 4 }
    )

    fun popTransitionSpec() = ContentTransform(
        targetContentEnter = fadeIn(tween(450, easing = exit)) +
                slideInHorizontally(tween(450, easing = exit)) { -(it * 0.75f).toInt() },
        initialContentExit = fadeOut(tween(450, easing = enter)) +
                slideOutHorizontally(tween(450, easing = enter)) { (it * 0.75f).toInt() }
    )
}
