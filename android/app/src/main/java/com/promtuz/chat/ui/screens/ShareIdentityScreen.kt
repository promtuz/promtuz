package com.promtuz.chat.ui.screens

import android.content.Intent
import androidx.camera.core.ExperimentalGetImage
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.wrapContentSize
import androidx.compose.material3.Button
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ShareIdentityVM
import com.promtuz.chat.ui.activities.QrScanner
import com.promtuz.chat.ui.components.BackTopBar
import com.promtuz.chat.ui.components.IdentityQrCode
import com.promtuz.chat.utils.InviteLink

@Composable
fun ShareIdentityScreen(
    viewModel: ShareIdentityVM
) {
    val colors = MaterialTheme.colorScheme

    Scaffold(
        topBar = { BackTopBar("Share Identity") }
    ) { innerPadding ->
        Box(
            Modifier
                .fillMaxSize()
                .padding(innerPadding)
                .background(colors.background),
        ) {
            Column(
                Modifier.fillMaxSize(),
                verticalArrangement = Arrangement.spacedBy(48.dp, Alignment.CenterVertically)
            ) {
                Box(
                    Modifier
                        .wrapContentSize()
                        .align(Alignment.CenterHorizontally)
                ) {
                    IdentityQrCode(viewModel.qrData.collectAsState())
                }

                Column(
                    Modifier.fillMaxWidth(),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(12.dp)
                ) {
                    ShareLinkButton(viewModel.qrData.collectAsState().value)
                    ScanQRButton()
                }
            }
        }
    }
}


@Composable
private fun ColumnScope.ShareLinkButton(inviteBytes: ByteArray?, modifier: Modifier = Modifier) {
    val context = LocalContext.current

    OutlinedButton(
        onClick = {
            val bytes = inviteBytes ?: return@OutlinedButton
            val link = InviteLink.build(bytes)
            val send = Intent(Intent.ACTION_SEND).apply {
                type = "text/plain"
                putExtra(Intent.EXTRA_TEXT, link)
            }
            context.startActivity(Intent.createChooser(send, "Share invite link"))
        },
        enabled = inviteBytes != null,
        modifier = modifier
            .fillMaxWidth(0.8f)
            .align(Alignment.CenterHorizontally),
    ) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Icon(
                painter = painterResource(R.drawable.i_send),
                contentDescription = "Share Link Icon"
            )

            Text(
                "Share link",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.labelLargeEmphasized
            )
        }
    }
}


@androidx.annotation.OptIn(ExperimentalGetImage::class)
@Composable
private fun ColumnScope.ScanQRButton(modifier: Modifier = Modifier) {
    val context = LocalContext.current

    Button(
        onClick = {
            context.startActivity(Intent(context, QrScanner::class.java))
        },
        modifier = modifier
            .fillMaxWidth(0.8f)
            .align(Alignment.CenterHorizontally),
    ) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Icon(
                painter = painterResource(R.drawable.i_qr_code_scanner),
                contentDescription = "QR Code Scanner Icon"
            )

            Text(
                "Scan QR Code",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.labelLargeEmphasized
            )
        }
    }
}
