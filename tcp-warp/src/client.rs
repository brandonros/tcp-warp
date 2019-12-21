use super::*;

pub struct TcpWarpClient {
    bind_address: IpAddr,
    server_address: SocketAddr,
    map: Vec<TcpWarpPortMap>,
}

impl TcpWarpClient {
    pub fn new(bind_address: IpAddr, server_address: SocketAddr, map: Vec<TcpWarpPortMap>) -> Self {
        Self {
            bind_address,
            server_address,
            map,
        }
    }

    pub async fn connect(&self) -> Result<(), Box<dyn Error>> {
        let stream = TcpStream::connect(&self.server_address).await?;
        let (mut wtransport, mut rtransport) = Framed::new(stream, TcpWarpProto).split();

        let (sender, mut receiver) = channel(100);

        let forward_task = async move {
            debug!("in receiver task");

            let mut connections = HashMap::new();

            while let Some(message) = receiver.next().await {
                debug!("just received a message connect: {:?}", message);
                let message = match message {
                    TcpWarpMessage::Connect {
                        connection_id,
                        sender,
                        host_port,
                        connected_sender,
                    } => {
                        debug!("adding connection: {}", connection_id);
                        connections.insert(
                            connection_id.clone(),
                            TcpWarpConnection {
                                sender,
                                connected_sender: Some(connected_sender),
                            },
                        );
                        TcpWarpMessage::HostConnect {
                            connection_id,
                            host_port,
                        }
                    }
                    TcpWarpMessage::DisconnectHost { ref connection_id } => {
                        if let Some(mut connection) = connections.remove(connection_id) {
                            if let Err(err) = connection.sender.send(message).await {
                                error!("cannot send to channel: {}", err);
                            }
                        } else {
                            error!("connection not found: {}", connection_id);
                        }
                        debug!("connections in pool: {}", connections.len());
                        continue;
                    }
                    TcpWarpMessage::Connected { ref connection_id } => {
                        if let Some(connection) = connections.get_mut(&connection_id) {
                            debug!("start connected loop: {}", connection_id);
                            if let Some(connection_sender) = connection.connected_sender.take() {
                                if let Err(err) = connection_sender.send(Ok(())) {
                                    error!("cannot send to oneshot channel: {:?}", err);
                                }
                            }
                        } else {
                            error!("connection not found: {}", connection_id);
                        }
                        continue;
                    }
                    TcpWarpMessage::BytesHost {
                        connection_id,
                        data,
                    } => {
                        if let Some(connection) = connections.get_mut(&connection_id) {
                            debug!(
                                "forward message to host port of connection: {}",
                                connection_id
                            );
                            if let Err(err) = connection
                                .sender
                                .send(TcpWarpMessage::BytesServer { data })
                                .await
                            {
                                error!("cannot send to channel: {}", err);
                            }
                        } else {
                            error!("connection not found: {}", connection_id);
                        }
                        continue;
                    }
                    regular_message => regular_message,
                };
                debug!("sending message {:?} from client to tunnel server", message);
                wtransport.send(message).await?;
            }

            debug!("no more messages, closing forward task");

            wtransport.close().await?;
            receiver.close();

            Ok::<(), io::Error>(())
        };

        let mapping = self.map.clone();
        let bind_address = self.bind_address;

        let processing_task = async move {
            while let Some(Ok(message)) = rtransport.next().await {
                process_host_to_client_message(message, sender.clone(), &mapping, bind_address)
                    .await?;
            }

            debug!("processing task for host to client finished");

            Ok::<(), io::Error>(())
        };

        let (_, _) = try_join!(forward_task, processing_task)?;

        Ok(())
    }
}

async fn process_host_to_client_message(
    message: TcpWarpMessage,
    mut sender: Sender<TcpWarpMessage>,
    mapping: &[TcpWarpPortMap],
    bind_address: IpAddr,
) -> Result<(), io::Error> {
    debug!("{} host to client: {:?}", bind_address, message);

    match message {
        TcpWarpMessage::AddPorts(host_ports) => {
            for host_port in host_ports {
                let port = client_port(mapping, host_port).unwrap_or(host_port);

                let bind_address = SocketAddr::new(bind_address, port);
                let sender_ = sender.clone();

                spawn(async move {
                    let mut listener = TcpListener::bind(bind_address).await?;

                    debug!("listen: {:?}", bind_address);

                    let mut incoming = listener.incoming();

                    while let Some(Ok(stream)) = incoming.next().await {
                        let sender__ = sender_.clone();

                        spawn(async move {
                            if let Err(e) = process(stream, sender__, host_port).await {
                                error!("failed to process connection; error = {}", e);
                            }
                        });
                    }

                    debug!("done listen: {:?}", bind_address);

                    Ok::<(), io::Error>(())
                });
            }
        }
        TcpWarpMessage::BytesHost { .. } => {
            if let Err(err) = sender.send(message).await {
                error!("cannot send message BytesHost to forward channel: {}", err);
            }
        }
        TcpWarpMessage::Connected { .. } => {
            if let Err(err) = sender.send(message).await {
                error!("cannot send message Connected to forward channel: {}", err);
            }
        }
        TcpWarpMessage::DisconnectHost { .. } => {
            if let Err(err) = sender.send(message).await {
                error!(
                    "cannot send message DisconnectHost to forward channel: {}",
                    err
                );
            }
        }
        other_message => warn!("unsupported message: {:?}", other_message),
    }
    Ok(())
}

fn client_port(mapping: &[TcpWarpPortMap], host_port: u16) -> Option<u16> {
    mapping
        .iter()
        .find(|x| x.host_port == host_port)
        .map(|x| x.client_port)
}

async fn process(
    stream: TcpStream,
    mut host_sender: Sender<TcpWarpMessage>,
    host_port: u16,
) -> Result<(), Box<dyn Error>> {
    let connection_id = Uuid::new_v4();

    debug!("new connection: {}", connection_id);

    let (mut wtransport, mut rtransport) =
        Framed::new(stream, TcpWarpProtoClient { connection_id }).split();

    let (client_sender, mut client_receiver) = channel(100);

    let forward_task = async move {
        debug!("in receiver task");
        while let Some(message) = client_receiver.next().await {
            debug!(
                "{} just received a message process: {:?}",
                connection_id, message
            );
            match message {
                TcpWarpMessage::DisconnectHost { .. } => break,
                TcpWarpMessage::BytesServer { data } => wtransport.send(data).await?,
                _ => (),
            }
        }

        debug!("{} no more messages, closing forward task", connection_id);
        debug!(
            "{} closing write channel to client side port",
            connection_id
        );
        wtransport.close().await?;
        client_receiver.close();
        debug!("{} write channel to client side port closed", connection_id);

        Ok::<(), io::Error>(())
    };

    let (connected_sender, connected_receiver) = oneshot::channel();

    host_sender
        .send(TcpWarpMessage::Connect {
            connection_id,
            host_port,
            sender: client_sender,
            connected_sender,
        })
        .await?;

    let processing_task = async move {
        if let Err(err) = connected_receiver.await {
            error!("{} connection error: {}", connection_id, err);
        }

        while let Some(Ok(message)) = rtransport.next().await {
            if let Err(err) = host_sender.send(message).await {
                error!("{} {}", connection_id, err);
            }
        }

        debug!(
            "{} processing task for incoming connection finished",
            connection_id
        );

        let message = TcpWarpMessage::DisconnectClient { connection_id };
        debug!("{} sending disconnect message {:?}", connection_id, message);
        if let Err(err) = host_sender.send(message).await {
            error!("{} {}", connection_id, err);
        }
        debug!("{} done processing", connection_id);

        Ok::<(), io::Error>(())
    };

    try_join!(forward_task, processing_task)?;

    debug!("{} full complete process", connection_id);

    Ok(())
}
