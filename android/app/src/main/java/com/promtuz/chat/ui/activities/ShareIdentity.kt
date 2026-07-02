package com.promtuz.chat.ui.activities

import android.os.Bundle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.appcompat.app.AppCompatActivity
import com.promtuz.chat.presentation.viewmodel.ShareIdentityVM
import com.promtuz.chat.ui.screens.ShareIdentityScreen
import com.promtuz.chat.ui.theme.PromtuzTheme
import com.promtuz.core.API
import org.koin.android.ext.android.inject
import org.koin.androidx.viewmodel.ext.android.viewModel

class ShareIdentity : AppCompatActivity() {
    private val viewModel: ShareIdentityVM by viewModel()
    private val api: API by inject()

    fun onQRCreate(qr: ByteArray) {
        viewModel.setQR(qr)
    }

//    fun onIdEvent(ev: IdentityEvent) {
//        when (ev) {
//            is IdentityEvent.AddMe -> viewModel.setIdReq(ev)
//        }
//    }

    override fun onDestroy() {
        super.onDestroy()
        api.identityDestroy()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        enableEdgeToEdge()

        api.identityInit(this)

        setContent {
            PromtuzTheme {
                ShareIdentityScreen(viewModel)
            }
        }
    }
}