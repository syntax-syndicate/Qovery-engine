data "aws_ami" "debian" {
  most_recent = true

  filter {
    name   = "name"
    values = [var.ec2_image_info.name]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }

  # to get owner id:
  # aws ec2 describe-images --image-ids <ami-id> --region us-west-2 | jq -r '.Images[0].OwnerId'
  owners = [var.ec2_image_info.owners]
}

{% if user_ssh_key != "" -%}
resource "aws_key_pair" "user_ssh_key" {
  key_name   = "qovery-${var.kubernetes_cluster_id}"
  public_key = "{{ user_ssh_key }}"
}
{%- endif %}

resource "aws_instance" "ec2_instance" {
  ami           = data.aws_ami.debian.id
  instance_type = var.ec2_instance.instance_type

  # root disk
  root_block_device {
    volume_size = var.ec2_instance.disk_size_in_gb
    volume_type = "gp2"
    encrypted = true
    delete_on_termination = true
    tags = merge(
      local.tags_common,
      {
        "Service" = "EC2"
      }
    )
  }

  # network
  associate_public_ip_address = true

  # security
  vpc_security_group_ids = [aws_security_group.ec2_instance.id]
  subnet_id = aws_subnet.ec2_zone_a[0].id

  {% if user_ssh_key != "" -%}
  # ssh
  key_name = aws_key_pair.user_ssh_key.key_name
  {%- endif %}

  # ebs csi driver + alb controller
  iam_instance_profile = aws_iam_instance_profile.aws_ebs_csi_driver.name

  # k3s install
  user_data = local.bootstrap
  user_data_replace_on_change = false

  tags = merge(
      local.tags_common,
      {
        "Service" = "EC2"
      }
    )

  # metadata
  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = var.ec2_metadata_imds_version
    # https://github.com/kubernetes/autoscaler/issues/3592
    # hop limit should be set to 2 for https://kubernetes-sigs.github.io/aws-load-balancer-controller/v2.4/deploy/installation/#using-the-amazon-ec2-instance-metadata-server-version-2-imdsv2
    http_put_response_hop_limit = 2
    instance_metadata_tags = "enabled"
  }

  depends_on = [
    aws_s3_bucket.kubeconfigs_bucket,
  ]
}

resource "time_static" "on_ec2_create" {}
# remove condition when migration is done
locals {
  bootstrap = <<BOOTSTRAP
{% if is_old_k3s_version -%}
#!/bin/bash

function print_title() {
  echo -e "\n######### $1 #########\n"
}

# enable logs to file and console
exec > >(tee ${var.ec2_instance.user_data_logs_path}|logger -t user_data -s 2>/dev/console) 2>&1

print_title "Wait for network to be available"
while ! ping -c 1 -W 1 8.8.8.8 1>/dev/null ; do
    echo "Waiting for network to be up..."
    sleep 1
done

print_title "Install packages"
apt-get update
apt-get -y install curl unzip

print_title "Setup Qovery SSH CA"
test $(grep -c qovery-ca.pem /etc/ssh/sshd_config) -eq 0 && echo "TrustedUserCAKeys /etc/ssh/qovery-ca.pem" >> /etc/ssh/sshd_config
curl -sL https://raw.githubusercontent.com/Qovery/ec2-system/main/qovery-ca.pem > /etc/ssh/qovery-ca.pem
chmod 600 /etc/ssh/qovery-ca.pem
systemctl restart ssh.service

print_title "Setup cron"
echo "*/15 * * * * root curl -sL https://raw.githubusercontent.com/Qovery/ec2-system/main/cron.sh > /etc/qovery/cron.sh && chmod 755 /etc/qovery/cron.sh && /etc/qovery/cron.sh" > /etc/cron.d/qovery
chmod 600 /etc/cron.d/qovery

print_title "Install latest aws cli version"
cd /tmp && curl "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip" -o /tmp/awscliv2.zip && unzip awscliv2.zip ./aws/install && cd -
echo 'export PATH=/usr/local/aws-cli/v2/current/bin:$PATH' >> /etc/profile

print_title "Install k3s"
export INSTALL_K3S_VERSION=${var.k3s_config.version}
export INSTALL_K3S_CHANNEL=${var.k3s_config.channel}
export INSTALL_K3S_EXEC="--https-listen-port=${random_integer.kubernetes_external_port.result} ${var.k3s_config.exec}"
echo "k3s agrs: $INSTALL_K3S_EXEC"
curl -fL https://get.k3s.io | sh -
echo 'export KUBECONFIG=/etc/rancher/k3s/k3s.yaml' >> /etc/profile

print_title "Create Qovery boot script"
mkdir /etc/qovery
cat << "EOF" > /etc/qovery/boot.sh
#!/bin/bash

KUBECONFIG_FILENAME="${var.kubernetes_cluster_id}.yaml"
NEW_KUBECONFIG_PATH="/tmp/$KUBECONFIG_FILENAME"
export AWS_ACCESS_KEY_ID={{ aws_access_key }}
export AWS_SECRET_ACCESS_KEY={{ aws_secret_key }}
export AWS_DEFAULT_REGION={{ aws_region }}

while [ ! -f /etc/rancher/k3s/k3s.yaml ] ; do
    echo "kubeconfig is not yet present, sleeping"
    sleep 1
done

public_hostname="$(TOKEN=`curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600"` \
&& curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/public-hostname)"
sed "s/127.0.0.1/$public_hostname/g" /etc/rancher/k3s/k3s.yaml > $NEW_KUBECONFIG_PATH
sed -i "s/:6443/:${random_integer.kubernetes_external_port.result}/g" $NEW_KUBECONFIG_PATH
aws s3 cp $NEW_KUBECONFIG_PATH s3://${var.s3_bucket_kubeconfig}/$KUBECONFIG_FILENAME --region {{ aws_region }}
EOF

print_title "Create Qovery systemd boot service"
cat << EOF > /etc/systemd/system/qovery-boot.service
[Unit]
Description=Qovery boot service

[Service]
ExecStart=/etc/qovery/boot.sh

[Install]
WantedBy=multi-user.target
EOF

print_title "Set permissions and start service"
chmod -R 700 /etc/qovery
chmod 755 /etc/systemd/system/qovery-boot.service
systemctl enable qovery-boot.service
systemctl start qovery-boot.service

print_title "Wait 5 min for k3s to start"
counter=0
max_counter=30
while [ $counter -lt $max_counter ] ; do
  systemctl is-active --quiet k3s
  if [ $? -eq 0 ] ; then
    echo "K3s has successfully started"
    exit 0
  fi
  ((counter=$counter+1))
  sleep 10
done

print_title "K3s failed to start, restarting the EC2 instance"
reboot





{%- else -%}
#!/bin/bash

function print_title() {
  echo -e "\n######### $1 #########\n"
}

print_title "Instance initialization"
# enable logs to file and console
exec > >(tee "${var.ec2_instance.user_data_logs_path}"|logger -t user_data -s 2>/dev/console) 2>&1

# remove file in order to ensure script will be run on each instance boot
rm -f /var/lib/cloud/instance/sem/config_scripts_user


print_title "Wait for network to be available"
while ! ping -c 1 -W 1 8.8.8.8 1>/dev/null ; do
    echo "Waiting for network to be up..."
    sleep 1
done


print_title "Install packages"
apt-get update
apt-get install -y curl unzip jq


print_title "Setup Qovery SSH CA"
test $(grep -c qovery-ca.pem /etc/ssh/sshd_config) -eq 0 && echo "TrustedUserCAKeys /etc/ssh/qovery-ca.pem" >> /etc/ssh/sshd_config
curl -sL https://raw.githubusercontent.com/Qovery/ec2-system/main/qovery-ca.pem > /etc/ssh/qovery-ca.pem
chmod 600 /etc/ssh/qovery-ca.pem
systemctl restart ssh.service


print_title "Install latest aws cli version"
{% if eks_worker_nodes[0].instance_architecture == "ARM64" -%}
cd /tmp && curl "https://awscli.amazonaws.com/awscli-exe-linux-aarch64.zip" -o /tmp/awscliv2.zip && unzip awscliv2.zip && ./aws/install && rm -rf aws awscliv2.zip && cd -
{%- else -%}
cd /tmp && curl "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip" -o /tmp/awscliv2.zip && unzip awscliv2.zip && ./aws/install && rm -rf aws awscliv2.zip && cd -
{%- endif %}
echo "export PATH=/usr/local/aws-cli/v2/current/bin:$PATH" >> /etc/profile


print_title "Setup cron"
echo "*/15 * * * * root curl -sL https://raw.githubusercontent.com/Qovery/ec2-system/main/cron.sh > /etc/qovery/cron.sh && chmod 755 /etc/qovery/cron.sh && /etc/qovery/cron.sh " > /etc/cron.d/qovery
chmod 700 /etc/cron.d/qovery


print_title "Install k3s"
export AWS_ACCESS_KEY_ID="{{ aws_access_key }}"
export AWS_SECRET_ACCESS_KEY="{{ aws_secret_key }}"
export AWS_DEFAULT_REGION="{{ aws_region }}"
# the cli returns the last deployed instance and wait for it to be running (code: 16)
while [ "$(aws ec2 describe-instances --filters Name=tag:ClusterId,Values="${var.kubernetes_cluster_id}" --query 'sort_by(Reservations[].Instances[], &LaunchTime)[-1:]' | jq -r .[0].State.Code)" -ne 16 ]; do
  echo 'Waiting for ec2 instance to start'
  sleep 5
done

# 169.254.169.254 is an internal static ip used by aws to retrieve nodes info without using the cli
# https://docs.aws.amazon.com/fr_fr/AWSEC2/latest/UserGuide/instancedata-data-retrieval.html
instance_id=$(TOKEN=`curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600"` \
&& curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/instance-id)
local_ip=$(TOKEN=`curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600"` \
&& curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/local-ipv4)
public_ip=$(TOKEN=`curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600"` \
&& curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/public-ipv4)
flannel_iface=$(ip -4 route ls | grep default | grep -Po '(?<=dev )(\S+)')
provider_id="$(TOKEN=`curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600"` \
&& curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/placement/availability-zone)/$(TOKEN=`curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600"` \
&& curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/instance-id)"
public_hostname_tls_san="$(TOKEN=`curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600"` \
&& curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/public-hostname)"

CUR_HOSTNAME=$(cat /etc/hostname)
NEW_HOSTNAME=$instance_id

hostnamectl set-hostname "$NEW_HOSTNAME"
hostname "$NEW_HOSTNAME"

sed -i "s/$CUR_HOSTNAME/$NEW_HOSTNAME/g" /etc/hosts
sed -i "s/$CUR_HOSTNAME/$NEW_HOSTNAME/g" /etc/hostname
# enforce k3s stability by switching to iptables-legacy
update-alternatives --set iptables /usr/sbin/iptables-legacy

chmod -R 700 /data/k3s

export INSTALL_K3S_VERSION=${var.k3s_config.version}
export INSTALL_K3S_CHANNEL=${var.k3s_config.channel}
export INSTALL_K3S_EXEC="--https-listen-port=${var.k3s_config.exposed_port} --disable=traefik --disable=metrics-server"
echo "k3s args: $INSTALL_K3S_EXEC"
while ! curl -sfL https://get.k3s.io | sh -s - --tls-san "$public_ip" --tls-san "$public_hostname_tls_san" --node-ip "$local_ip" --advertise-address "$local_ip" --flannel-iface "$flannel_iface" --kubelet-arg="cloud-provider=external" --kubelet-arg="provider-id=aws:///$provider_id" --etcd-arg "--advertise-client-urls" ; do
  echo 'k3s did not install correctly'
  sleep 2
done


print_title "Wait for k3s to start"
while [ $(kubectl get pods -A | grep -E -w  'coredns|local-path-provisioner' | grep -c 'Running' ) -lt 2 ]; do
  echo 'Waiting for k3s startup'
  sleep 5
done

echo "export KUBECONFIG=/etc/rancher/k3s/k3s.yaml" >> /etc/profile


print_title "Create Qovery boot script"
mkdir /etc/qovery
chmod 777 /etc/qovery
cat << "EOF" > /etc/qovery/boot.sh
#!/bin/bash

KUBECONFIG_FILENAME="${var.kubernetes_cluster_id}.yaml"
NEW_KUBECONFIG_PATH="/tmp/$KUBECONFIG_FILENAME"
export AWS_ACCESS_KEY_ID="{{ aws_access_key }}"
export AWS_SECRET_ACCESS_KEY="{{ aws_secret_key }}"
export AWS_DEFAULT_REGION="{{ aws_region }}"

while [ ! -f /etc/rancher/k3s/k3s.yaml ] ; do
    echo "kubeconfig is not yet present, sleeping"
    sleep 1
done

public_hostname="$(TOKEN=`curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600"` \
&& curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/public-hostname)"
sed "s/127.0.0.1/$public_hostname/g" /etc/rancher/k3s/k3s.yaml > "$NEW_KUBECONFIG_PATH"
sed -i "s/:6443/:${var.k3s_config.exposed_port}/g" "$NEW_KUBECONFIG_PATH"
aws s3 cp "$NEW_KUBECONFIG_PATH" s3://${var.s3_bucket_kubeconfig}/$KUBECONFIG_FILENAME --region "{{ aws_region }}"
EOF

chmod 700 /etc/qovery/boot.sh


print_title "Create Qovery systemd boot service"
cat << EOF > /etc/systemd/system/qovery-boot.service
[Unit]
Description=Qovery boot service

[Service]
ExecStart=/etc/qovery/boot.sh

[Install]
WantedBy=multi-user.target
EOF


print_title "Set permissions and start service"
chmod 644 /etc/systemd/system/qovery-boot.service
systemctl enable qovery-boot.service
systemctl start qovery-boot.service

{%- endif %}
BOOTSTRAP
}
