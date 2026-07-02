package com.promtuz.chat.ui.components

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.LoadingIndicator
import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import com.promtuz.chat.ui.views.QrView

@Composable
fun IdentityQrCode(
    data: State<ByteArray?>, modifier: Modifier = Modifier
) {
    val context = LocalContext.current

    // TODO: make these customizable as well
    val containerColor = Color.White
    val modulesColor = Color.Black

    val data by data
    val loading by remember { derivedStateOf { data == null } }
    val qrView = remember { QrView(context) }

    Box(
        modifier
            .padding(48.dp)
            .clip(RoundedCornerShape(12))
            .background(containerColor)
            .padding(32.dp)
            .aspectRatio(1f)
    ) {
        AnimatedVisibility(loading, Modifier.align(Alignment.Center)) {
            LoadingIndicator(
                Modifier
                    .fillMaxSize(0.4f)
            )
        }

        AndroidView(factory = { qrView }, update = { v ->
            v.loading = loading
            if (data == null) {
                v.clear()
            } else {
                data?.let { v.content = it }
                v.regenerate()
            }
            v.color = modulesColor.toArgb()
            v.regenerate()
        })
    }
}