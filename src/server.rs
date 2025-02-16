use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use pyo3::{prelude::*, types::PyTuple};
use pyo3::exceptions::PyValueError;
use tokio::{
    net::UdpSocket,
    sync::broadcast::{self, Sender as BroadcastSender},
    sync::mpsc::{self, channel, unbounded_channel},
    sync::Notify,
};
use x25519_dalek::{PublicKey};

use crate::messages::TransportCommand;
use crate::network::NetworkTask;
use crate::python::{event_queue_unavailable, py_to_socketaddr, socketaddr_to_py, PyInteropTask, TcpStream};
use crate::shutdown::ShutdownTask;
use crate::util::string_to_key;
use crate::wireguard::WireGuardTaskBuilder;

/// A running WireGuard server.
///
/// A new server can be started by calling the `start_server` coroutine. Its public API is intended
/// to be similar to the API provided by
/// [`asyncio.Server`](https://docs.python.org/3/library/asyncio-eventloop.html#asyncio.Server)
/// from the Python standard library.
#[pyclass]
#[derive(Debug)]
pub struct Server {
    /// queue of events to be sent to the Python interop task
    event_tx: mpsc::UnboundedSender<TransportCommand>,
    /// local address of the WireGuard UDP socket
    local_addr: SocketAddr,
    /// channel for notifying subtasks of requested server shutdown
    sd_trigger: BroadcastSender<()>,
    /// channel for getting notified of successful server shutdown
    sd_barrier: Arc<Notify>,
    /// flag to indicate whether server shutdown is in progress
    closing: bool,
}

#[pymethods]
impl Server {
    pub fn new_connection<'p>(
        &self,
        py: Python<'p>,
        src_addr: &PyTuple,
        dst_addr: &PyTuple,
    ) -> PyResult<&'p PyAny> {
        let src_addr = py_to_socketaddr(src_addr)?;
        let dst_addr = py_to_socketaddr(dst_addr)?;

        let stream_event_tx = self.event_tx.clone();

        pyo3_asyncio::tokio::future_into_py(py, async move {
            let stream = TcpStream::new(stream_event_tx, src_addr, dst_addr).await?;

            Python::with_gil(|py| -> PyResult<PyObject> {
                let stream = stream.into_py(py);

                Ok(stream)
            })
        })
    }

    /// Send an individual UDP datagram using the specified source and destination addresses.
    ///
    /// The `src_addr` and `dst_addr` arguments are expected to be `(host: str, port: int)` tuples.
    pub fn send_datagram(
        &self,
        data: Vec<u8>,
        src_addr: &PyTuple,
        dst_addr: &PyTuple,
    ) -> PyResult<()> {
        let cmd = TransportCommand::SendDatagram {
            data,
            src_addr: py_to_socketaddr(src_addr)?,
            dst_addr: py_to_socketaddr(dst_addr)?,
        };

        self.event_tx.send(cmd).map_err(event_queue_unavailable)?;
        Ok(())
    }

    /// Send an raw IP packet on the socket.
    pub fn send_other_packet(
        &self,
        data: Vec<u8>,
    ) -> PyResult<()> {
        let cmd = TransportCommand::SendOtherPacket {
            data,
        };

        self.event_tx.send(cmd).map_err(event_queue_unavailable)?;
        Ok(())
    }


    /// Request the WireGuard server to gracefully shut down.
    ///
    /// The server will stop accepting new connections on its UDP socket, but will flush pending
    /// outgoing data before shutting down.
    pub fn close(&mut self) {
        if !self.closing {
            self.closing = true;
            log::info!("Shutting down.");

            // notify tasks to shut down
            let _ = self.sd_trigger.send(());
        }
    }

    /// Wait until the WireGuard server has shut down.
    ///
    /// This coroutine will yield once pending data has been flushed and all server tasks have
    /// successfully terminated after calling the `Server.close` method.
    pub fn wait_closed<'p>(&self, py: Python<'p>) -> PyResult<&'p PyAny> {
        let barrier = self.sd_barrier.clone();

        pyo3_asyncio::tokio::future_into_py(py, async move {
            barrier.notified().await;
            Ok(())
        })
    }

    /// Get the local socket address that the WireGuard server is listening on.
    pub fn getsockname(&self, py: Python) -> PyObject {
        socketaddr_to_py(py, self.local_addr)
    }

    pub fn __repr__(&self) -> String {
        format!("Server({})", self.local_addr)
    }
}

impl Server {
    /// Set up and initialize a new WireGuard server.
    pub async fn init(
        host: String,
        port: u16,
        private_key: String,
        peer_public_keys: Vec<String>,
        peer_endpoints: Vec<Option<String>>,
        py_tcp_handler: PyObject,
        py_udp_handler: PyObject,
        py_other_packet_handler: PyObject,
    ) -> Result<Self> {
        log::debug!("Initializing WireGuard server ...");

        // initialize channels between the WireGuard server and the virtual network device
        let (wg_to_smol_tx, wg_to_smol_rx) = channel(256);
        let (smol_to_wg_tx, smol_to_wg_rx) = channel(256);

        // initialize channels between the virtual network device and the python interop task
        // - only used to notify of incoming connections and datagrams
        let (smol_to_py_tx, smol_to_py_rx) = channel(256);
        // - used to send data and to ask for packets
        // This channel needs to be unbounded because write() is not async.
        let (py_to_smol_tx, py_to_smol_rx) = unbounded_channel();

        let event_tx = py_to_smol_tx.clone();

        // bind to UDP socket(s)
        let socket_addrs = if host.is_empty() {
            vec![
                SocketAddr::new("0.0.0.0".parse().unwrap(), port),
                SocketAddr::new("::".parse().unwrap(), port),
            ]
        } else {
            vec![SocketAddr::new(host.parse()?, port)]
        };

        let socket = UdpSocket::bind(socket_addrs.as_slice()).await?;
        let local_addr = socket.local_addr()?;

        log::debug!(
            "WireGuard server listening for UDP connections on {} ...",
            socket_addrs
                .iter()
                .map(|addr| addr.to_string())
                .collect::<Vec<String>>()
                .join(" and ")
        );

        // initialize barriers for handling graceful shutdown
        let (sd_trigger, _sd_watcher) = broadcast::channel(1);
        let sd_barrier = Arc::new(Notify::new());

        // load keys
        let private_key = string_to_key(private_key)?;
        let peer_public_keys = peer_public_keys
            .into_iter()
            .map(string_to_key)
            .collect::<PyResult<Vec<PublicKey>>>()?;

        // resolve endpoints if any
        let peer_endpoints = peer_endpoints
            .into_iter()
            .map(|endpoint| -> PyResult<Option<SocketAddr>> {
                match endpoint {
                    Some(s) => Ok(Some(
                        s.to_socket_addrs()?
                            // filter out IPv4 results if local socket is IPv4, and IPv6 if local
                            // address is IPv6
                            .filter(|a| local_addr.is_ipv4() && a.is_ipv4()
                                || local_addr.is_ipv6() && a.is_ipv6())
                            .next().ok_or(|| PyValueError::new_err("Endpoint Host not found"))
                            .map_err(|_| PyValueError::new_err("Invalid endpoint."))?
                    )),
                    None => Ok(None)
                }
            })
            .collect::<PyResult<Vec<Option<SocketAddr>>>>()?;

        if peer_public_keys.len() != peer_endpoints.len() {
            return Err(anyhow!("Peer public key and endpoint lists don't match"));
        }

        // initialize WireGuard server
        let mut wg_task_builder = WireGuardTaskBuilder::new(
            private_key,
            wg_to_smol_tx,
            smol_to_wg_rx,
            sd_trigger.subscribe(),
        );
        for (key, endpoint) in peer_public_keys.into_iter().zip(peer_endpoints.into_iter()) {
            wg_task_builder.add_peer(key, None, endpoint)?;
        }
        let wg_task = wg_task_builder.build()?;

        // initialize virtual network device
        let nw_task = NetworkTask::new(
            smol_to_wg_tx,
            wg_to_smol_rx,
            smol_to_py_tx,
            py_to_smol_rx,
            sd_trigger.subscribe(),
        )?;

        // initialize Python interop task
        // Note: The current asyncio event loop needs to be determined here on the main thread.
        let py_loop: PyObject = Python::with_gil(|py| {
            let py_loop = pyo3_asyncio::tokio::get_current_loop(py)?.into_py(py);
            Ok::<PyObject, PyErr>(py_loop)
        })?;

        let py_task = PyInteropTask::new(
            local_addr,
            py_loop,
            py_to_smol_tx,
            smol_to_py_rx,
            py_tcp_handler,
            py_udp_handler,
            py_other_packet_handler,
            sd_trigger.subscribe(),
        );

        // spawn tasks
        let wg_handle = tokio::spawn(async move { wg_task.run(socket).await });
        let net_handle = tokio::spawn(async move { nw_task.run().await });
        let py_handle = tokio::spawn(async move { py_task.run().await });

        // initialize and run shutdown handler
        let sd_task = ShutdownTask::new(
            py_handle,
            wg_handle,
            net_handle,
            sd_trigger.clone(),
            sd_barrier.clone(),
        );
        tokio::spawn(async move { sd_task.run().await });

        log::debug!("WireGuard server successfully initialized.");

        Ok(Server {
            event_tx,
            local_addr,
            sd_trigger,
            sd_barrier,
            closing: false,
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.close()
    }
}

/// Start a WireGuard server that is configured with the given parameters:
///
/// - `host`: The host address for the WireGuard UDP socket.
/// - `port`: The listen port for the WireGuard server. The default port for WireGuard is `51820`.
/// - `private_key`: The private X25519 key for the WireGuard server as a base64-encoded string.
/// - `peer_public_keys`: List of public X25519 keys for WireGuard peers as base64-encoded strings.
/// - `peer_endpoints`: List of default endpoints for WireGuard peers. Each element must be present, but can be None.
/// - `handle_connection`: A coroutine that will be called for each new `TcpStream`.
/// - `receive_datagram`: A function that will be called for each received UDP datagram.
/// - `receive_other`: A function that will be called for each received IP packet that is neither TCP or UDP.
///
/// The `receive_datagram` function will be called with the following arguments:
///
/// - payload of the UDP datagram as `bytes`
/// - source address as `(host: str, port: int)` tuple
/// - destination address as `(host: str, port: int)` tuple
#[pyfunction]
pub fn start_server(
    py: Python<'_>,
    host: String,
    port: u16,
    private_key: String,
    peer_public_keys: Vec<String>,
    peer_endpoints: Vec<Option<String>>,
    handle_connection: PyObject,
    receive_datagram: PyObject,
    receive_other_packet: PyObject,
) -> PyResult<&PyAny> {
    pyo3_asyncio::tokio::future_into_py(py, async move {
        let server = Server::init(
            host,
            port,
            private_key,
            peer_public_keys,
            peer_endpoints,
            handle_connection,
            receive_datagram,
            receive_other_packet,
        )
        .await?;
        Ok(server)
    })
}
