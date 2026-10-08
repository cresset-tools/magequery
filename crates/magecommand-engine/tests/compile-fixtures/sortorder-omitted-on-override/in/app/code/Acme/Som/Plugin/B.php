<?php

namespace Acme\Som\Plugin;

class B
{
    public function afterAct($subject, int $result): int
    {
        return $result;
    }
}
