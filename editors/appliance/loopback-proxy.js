// Loopback→backend TCP forwarder for the two-container appliance.
//
// The Lens extension's discovery contract accepts LOOPBACK URLs only (a discovery file
// pointing off-loopback is a hard boundary violation). The backend lives in the sibling
// `serve` container, so this forwarder makes it appear at 127.0.0.1:18120 from the
// workbench's view — discovery stays loopback-shaped and the extension dials it
// happily, while the DB mount still lives exclusively in the serve container.
const net = require('net');

const targetHost = process.env.LENS_BACKEND_HOST || 'serve';
const targetPort = Number(process.env.LENS_BACKEND_PORT || 18120);
const listenPort = Number(process.env.LENS_LOOPBACK_PORT || 18120);
// The workbench sidecar binds loopback (the extension dials 127.0.0.1); the `edge`
// service binds all interfaces (docker-proxy forwards host traffic to the container IP).
const listenHost = process.env.LENS_LISTEN_HOST || '127.0.0.1';

net
  .createServer((client) => {
    const upstream = net.connect(targetPort, targetHost);
    client.pipe(upstream).pipe(client);
    const drop = () => {
      client.destroy();
      upstream.destroy();
    };
    client.on('error', drop);
    upstream.on('error', drop);
  })
  .listen(listenPort, listenHost);
