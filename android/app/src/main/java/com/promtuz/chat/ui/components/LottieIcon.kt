package com.promtuz.chat.ui.components

import androidx.annotation.RawRes
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import com.airbnb.lottie.LottieComposition
import com.airbnb.lottie.compose.LottieAnimation
import com.airbnb.lottie.compose.LottieCompositionSpec
import com.airbnb.lottie.compose.LottieConstants
import com.airbnb.lottie.compose.animateLottieCompositionAsState
import com.airbnb.lottie.compose.rememberLottieComposition

@Composable
fun rememberLottie(@RawRes res: Int): LottieComposition? =
    rememberLottieComposition(LottieCompositionSpec.RawRes(res)).value

/** Draws the composition parked at [frame]; the caller owns the motion between marker frames. */
@Composable
fun LottieFrame(composition: LottieComposition?, frame: () -> Float, modifier: Modifier = Modifier) {
    LottieAnimation(
        composition,
        progress = {
            val c = composition ?: return@LottieAnimation 0f
            ((frame() - c.startFrame) / (c.endFrame - c.startFrame)).coerceIn(0f, 1f)
        },
        modifier = modifier,
    )
}

/** A composition whose whole timeline is one loop. */
@Composable
fun LottieLoop(@RawRes res: Int, modifier: Modifier = Modifier) {
    val composition = rememberLottie(res)
    val progress by animateLottieCompositionAsState(composition, iterations = LottieConstants.IterateForever)
    LottieAnimation(composition, progress = { progress }, modifier = modifier)
}
