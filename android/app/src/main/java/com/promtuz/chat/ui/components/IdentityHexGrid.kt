package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign

@Composable
fun IdentityHexGrid(
    key: ByteArray,
    textStyle: TextStyle = MaterialTheme.typography.titleLargeEmphasized
) {
    val keyHex = key.toHexString(HexFormat.UpperCase)

    // TODO: add this in "appearance" settings as well
//    val contentColor = Color.Black

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .aspectRatio(1f),
    ) {
        for (r in 0 until 8) {
            Row(Modifier.weight(1f)) {
                for (c in 0 until 8) {
                    val ch = keyHex[r * 8 + c].toString()
                    Box(
                        modifier = Modifier
                            .weight(1f)
                            .fillMaxHeight(),
                        contentAlignment = Alignment.Center
                    ) {
                        Text(
                            ch,
                            style = textStyle.copy(
                                color = LocalContentColor.current,
                                fontWeight = FontWeight.Bold
                            ),
                            textAlign = TextAlign.Center
                        )
                    }
                }
            }
        }
    }
}